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

/// The marker on the row being moved.
///
/// Must be exactly as wide as [`MARKER`], or every name jumps a column at the
/// moment the eye is following one of them. `⇕` (U+21D5) rather than the more
/// obvious `↕` (U+2195): the latter carries the Emoji property, so a terminal
/// applying emoji presentation paints it two columns wide while `unicode-width`
/// still reports one — the very defect that blanking the number column, rather
/// than collapsing it, is there to avoid.
const GRABBED: &str = "⇕ ";

/// The prompt cursor.
const CURSOR: &str = "▋";

/// Only a floor for the degenerate case of every name being empty; the block is
/// otherwise sized to its content. A minimum wider than the content would push
/// short names visibly left of centre, because names are left-aligned *within*
/// the block so they line up with each other.
const MIN_LIST_WIDTH: u16 = 4;
const MAX_LIST_WIDTH: u16 = 48;

/// Columns between the number and the name.
const NUM_GAP: &str = "  ";

/// Sixty-nine columns, and it used to be sixty exactly — the widest row that
/// still fits a small terminal without truncation. `␣ order` is what that budget
/// was spent on: reordering is the one picker command nobody would find unaided,
/// since it leaves no trace on screen until it is used and `?` shows the
/// `<prefix>` keys rather than these.
///
/// So below sixty-nine columns this row now truncates from the right, where
/// before it never did. `q quit` is therefore the first entry to go, which is
/// the order to want: quitting is the one thing every user of a full-screen
/// program tries unprompted, and `Ctrl-c` quits as well.
///
/// The space bar is `␣` rather than the `Space` that [`crate::keys::key_label`]
/// spells it everywhere else, because this row is uniformly lowercase (`esc`,
/// never `Esc`) and a glyph is neither — the same reason `⏎` and `↑↓` stand for
/// Enter and the arrows here and are written out in the README.
///
/// `?` is still not listed, for the reason above.
const HINTS: &str = "↑↓ move  ⏎ attach  c new  r rename  x kill  ␣ order  / filter  q quit";
const EMPTY: &str = "no sessions — press c to create one";

/// What the hint row says while a session is in flight. Undimmed, like the
/// filter and the kill confirm: a mode holding an unwritten edit must not look
/// like the ambient reminder of keys.
///
/// `␣ place` is the same glyph the normal row spends on `␣ order`, and says
/// so: the key that picked the session up is the one that puts it down.
const REORDER_HINTS: &str = "↑↓ move  ␣ place  esc cancel";

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    if area.height == 0 || area.width == 0 {
        return;
    }
    let (list_area, bottom) = split_hint_row(area);
    draw_list(frame, app, list_area);
    draw_bottom(frame, app, bottom);
}

/// The layout every screen shares: the body above, one hint row on the last
/// line. Same split everywhere, so the screens line up across a transition.
pub(super) fn split_hint_row(area: Rect) -> (Rect, Rect) {
    let body = Rect {
        height: area.height.saturating_sub(1),
        ..area
    };
    let bottom = Rect {
        y: area.y + area.height - 1,
        height: 1,
        ..area
    };
    (body, bottom)
}

/// The one-row line at the bottom of every screen: centred, truncated rather
/// than wrapped (the row owns exactly one line, and wrapping would push the
/// body up and make the layout jump), dim when it is a hint rather than
/// something the user is being told.
pub(super) fn draw_hint_row(frame: &mut Frame, area: Rect, text: &str, dim: bool) {
    let text = truncate(text, area.width as usize);
    let style = if dim {
        Style::default().add_modifier(Modifier::DIM)
    } else {
        Style::default()
    };
    let para = Paragraph::new(Line::from(Span::styled(text, style))).alignment(Alignment::Center);
    frame.render_widget(para, area);
}

fn draw_list(frame: &mut Frame, app: &App, area: Rect) {
    if area.height == 0 {
        return;
    }
    let visible = app.visible();

    if visible.is_empty() {
        let text = truncate(EMPTY, area.width as usize);
        let para = Paragraph::new(Line::from(Span::styled(
            text,
            Style::default().add_modifier(Modifier::DIM),
        )))
        .alignment(Alignment::Center);
        frame.render_widget(para, centre_vertically(area, 1));
        return;
    }

    let widest = visible.iter().map(|s| s.name.width()).max().unwrap_or(0) as u16;
    // Right-aligned in a column as wide as the longest number, so the names stay
    // in one column once the list runs past nine.
    let num_width = visible
        .iter()
        .map(|s| s.state.num.to_string().len())
        .max()
        .unwrap_or(1);
    let width = (widest + MARKER.width() as u16 + num_width as u16 + NUM_GAP.width() as u16)
        .clamp(MIN_LIST_WIDTH, MAX_LIST_WIDTH)
        .min(area.width);

    let height = (visible.len() as u16).min(area.height);
    let block = centre(area, width, height);

    let offset = scroll_offset(app.selected_index(), visible.len(), height as usize);

    // The number column is kept but left blank while a session is being moved.
    // Dropping it outright would narrow the centred block by `num_width` plus
    // the gap and slide every name sideways exactly as one of them is being
    // watched — the same thing the matching indent on unselected rows refuses.
    // Hiding the digits is a display choice and not a correctness one: the
    // numbers stay where they are on screen throughout a move, and it is the
    // sessions that travel between them. They go away because the digit keys are
    // dead in this mode, and a column of numbers changing owner under a moving
    // row is noise.
    let reordering = matches!(app.mode(), Mode::Reorder { .. });

    let rows: Vec<Line> = visible
        .iter()
        .enumerate()
        .skip(offset)
        .take(height as usize)
        .map(|(i, session)| {
            let selected = i == app.selected_index();
            let prefix = match (selected, reordering) {
                (true, true) => GRABBED,
                (true, false) => MARKER,
                (false, _) => INDENT,
            };
            let style = if selected {
                Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                Style::default()
            };
            let num = if reordering {
                String::new()
            } else {
                session.state.num.to_string()
            };
            let text = truncate(
                &format!("{prefix}{num:>num_width$}{NUM_GAP}{}", session.name),
                block.width as usize,
            );
            Line::from(Span::styled(text, style))
        })
        .collect();

    frame.render_widget(Paragraph::new(rows), block);
}

fn draw_bottom(frame: &mut Frame, app: &App, area: Rect) {
    let (text, dim) = match app.mode() {
        Mode::Confirm { prompt, .. } => (prompt.clone(), false),
        Mode::Filter => (format!("/{}{CURSOR}", app.filter()), false),
        // The query comes too, when there is one. A move of one visible row can
        // carry a session past several the filter is hiding, and that is the one
        // moment the screen must not also stop saying a filter is applied — the
        // reason the dim `/query` below exists at all.
        Mode::Reorder { .. } if !app.filter().is_empty() => {
            (format!("{REORDER_HINTS}  /{}", app.filter()), false)
        }
        Mode::Reorder { .. } => (REORDER_HINTS.to_string(), false),
        Mode::Normal => match app.message() {
            Some(msg) => (msg.to_string(), false),
            // A number waiting on another digit; without this the picker would
            // look like it had ignored the keystroke.
            None => match app.pending() {
                Some(n) => (format!("#{n}{CURSOR}"), false),
                None if !app.filter().is_empty() => {
                    // Or the list silently looks shorter than it is.
                    (format!("/{}", app.filter()), true)
                }
                None => (HINTS.to_string(), true),
            },
        },
    };

    draw_hint_row(frame, area, &text, dim);
}

/// Keep `selected` visible within a window of `height` rows.
pub(super) fn scroll_offset(selected: usize, total: usize, height: usize) -> usize {
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
pub(crate) fn truncate(s: &str, max: usize) -> String {
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
    use super::super::test_support;
    use super::*;
    use crate::session::Session;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render(app: &App, w: u16, h: u16) -> Vec<String> {
        test_support::render(w, h, |f| draw(f, app))
    }

    /// Numbered the way `finish_listing` would have: these `App`s are built
    /// directly, so nothing else would set the resolved number.
    fn app(names: &[&str]) -> App {
        App::new(
            names
                .iter()
                .enumerate()
                .map(|(i, n)| {
                    let num = i as u32 + 1;
                    let mut s =
                        Session::new(format!("id{i:06}"), n.to_string(), 100 + i as u32, num);
                    s.state.num = num;
                    s
                })
                .collect(),
        )
    }

    #[test]
    fn the_whole_screen_is_a_centred_list_and_one_hint_line() {
        // Seventy columns because `HINTS` is sixty-nine: narrower and the row
        // is truncated, and "the hints are on the last row" would be asserted
        // against whatever happened to survive.
        let lines = render(
            &app(&["api-server", "dotfiles", "scratch", "notes"]),
            70,
            11,
        );

        assert!(
            lines[10].contains("attach") && lines[10].contains("quit"),
            "hints should be on the last row, got {:?}",
            lines[10]
        );

        let content: Vec<&String> = lines[..10].iter().filter(|l| !l.is_empty()).collect();
        assert_eq!(content.len(), 4, "expected 4 rows, got {content:?}");

        test_support::assert_no_borders(&lines);
    }

    #[test]
    fn the_selection_is_marked_and_others_are_indented_to_match() {
        let lines = render(&app(&["aaa", "bbb"]), 40, 6);
        let rows: Vec<&String> = lines
            .iter()
            .filter(|l| l.contains("aa") || l.contains("bb"))
            .collect();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].contains("▸ 1  aaa"), "selected row: {:?}", rows[0]);
        assert!(
            rows[1].contains("  2  bbb"),
            "unselected row: {:?}",
            rows[1]
        );

        // Display columns, not bytes: "▸" is three bytes but one column.
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

        assert_eq!(occupied, vec![9, 10], "list is not vertically centred");

        let row = &lines[9];
        let left = row.len() - row.trim_start().len();
        let right = 60 - row.width();
        assert!(
            left.abs_diff(right) <= 2,
            "not horizontally centred: {left} left, {right} right, row {row:?}"
        );
    }

    #[test]
    fn every_row_carries_its_session_number() {
        let lines = render(&app(&["api-server", "dotfiles", "notes"]), 40, 6);
        for want in ["▸ 1  api-server", "  2  dotfiles", "  3  notes"] {
            assert!(
                lines.iter().any(|l| l.contains(want)),
                "missing {want:?} in {lines:#?}"
            );
        }
    }

    /// The block is sized to fit the number column, or the names it was
    /// measured for would be truncated by exactly the width of the number.
    #[test]
    fn the_list_block_grows_to_fit_the_number_column() {
        let lines = render(&app(&["exactly-this-long"]), 40, 6);
        let row = lines
            .iter()
            .find(|l| l.contains("exactly"))
            .expect("the row");
        assert!(
            row.contains("▸ 1  exactly-this-long"),
            "the name was truncated to make room: {row:?}"
        );
    }

    /// A half-typed number is shown, or the picker looks like it swallowed the
    /// keystroke.
    #[test]
    fn a_pending_number_is_shown_on_the_hint_row() {
        let names: Vec<String> = (0..12).map(|i| format!("s{i:02}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut a = app(&refs);
        a.on_key(super::super::app::Key::Char('1'));

        let lines = render(&a, 40, 14);
        assert!(
            lines[13].contains("#1"),
            "the pending number is not on the hint row: {:?}",
            lines[13]
        );
    }

    /// A session picked up for reordering, with the cursor on `row`.
    fn reordering(names: &[&str], row: usize) -> App {
        let mut a = app(names);
        for _ in 0..row {
            a.on_key(super::super::app::Key::Char('j'));
        }
        a.on_key(super::super::app::Key::Char(' '));
        assert!(matches!(a.mode(), Mode::Reorder { .. }), "the grab took");
        a
    }

    /// The numbers go away while a session is in flight, as asked: the digit
    /// keys are dead in this mode and a column of numbers changing owner under a
    /// moving row is noise.
    #[test]
    fn the_numbers_are_hidden_while_a_session_is_being_moved() {
        let lines = render(&reordering(&["api-server", "dotfiles", "notes"], 0), 40, 6);
        for line in lines.iter().filter(|l| l.contains("dotfiles")) {
            assert!(
                !line.chars().any(|c| c.is_ascii_digit()),
                "a number survived into reorder mode: {line:?}"
            );
        }
        assert!(lines.iter().any(|l| l.contains("api-server")), "{lines:#?}");
    }

    /// The column is blanked, not removed. Collapsing it would narrow the
    /// centred block and slide every name sideways at the exact moment the eye
    /// is following one of them.
    #[test]
    fn the_names_do_not_move_when_a_session_is_picked_up() {
        let column = |a: &App| -> Vec<usize> {
            render(a, 40, 6)
                .iter()
                .filter(|l| l.contains("dotfiles") || l.contains("notes"))
                .map(|l| l[..l.find(char::is_alphabetic).expect("a name")].width())
                .collect()
        };
        let before = column(&app(&["dotfiles", "notes"]));
        let after = column(&reordering(&["dotfiles", "notes"], 0));
        assert_eq!(before.len(), 2);
        assert_eq!(
            before, after,
            "the names shifted when the row was picked up"
        );
    }

    #[test]
    fn the_grabbed_row_is_marked_differently_from_the_cursor() {
        assert_eq!(
            GRABBED.width(),
            MARKER.width(),
            "a marker of a different width shifts the whole name column"
        );
        assert_eq!(MARKER.width(), INDENT.width());

        let lines = render(&reordering(&["aaa", "bbb"], 1), 40, 6);
        let row = lines.iter().find(|l| l.contains("bbb")).expect("the row");
        assert!(row.contains(GRABBED), "grabbed row: {row:?}");
        assert!(
            !lines.iter().any(|l| l.contains(MARKER)),
            "the plain cursor marker is still on screen: {lines:#?}"
        );
    }

    /// One visible row of movement can carry a session past several the filter
    /// is hiding. That is the one moment the screen must not also stop saying a
    /// filter is applied.
    #[test]
    fn an_applied_filter_is_still_shown_while_reordering() {
        let mut a = app(&["api-server", "dotfiles"]);
        a.on_key(super::super::app::Key::Char('/'));
        for c in "dot".chars() {
            a.on_key(super::super::app::Key::Char(c));
        }
        a.on_key(super::super::app::Key::Enter); // filter applied, normal mode
        a.on_key(super::super::app::Key::Char(' '));

        let lines = render(&a, 60, 6);
        assert!(lines[5].contains("place"), "got {:?}", lines[5]);
        assert!(
            lines[5].contains("/dot"),
            "the query vanished: {:?}",
            lines[5]
        );
    }

    #[test]
    fn the_empty_state_is_one_dimmed_line_with_hints_still_below() {
        let lines = render(&app(&[]), 70, 9);
        let content: Vec<&String> = lines[..8].iter().filter(|l| !l.is_empty()).collect();
        assert_eq!(content.len(), 1, "expected one line, got {content:?}");
        assert!(content[0].contains("no sessions"), "got {:?}", content[0]);
        assert!(content[0].contains("press c to create one"));
        assert!(lines[8].contains("quit"), "hints should still be present");
    }

    /// The picker's own prompts — the filter and the kill confirm — stay on the
    /// hint row: no popup, no border, no shift in the list above. Naming is the
    /// exception, and hands off to the full-screen prompt.
    #[test]
    fn prompts_replace_the_hint_line_in_place() {
        for (open_with, typed, expected) in
            [('/', "dot", "/dot▋"), ('x', "", "kill \"dotfiles\"? [y/N]")]
        {
            let mut a = app(&["dotfiles"]);
            a.on_key(super::super::app::Key::Char(open_with));
            for c in typed.chars() {
                a.on_key(super::super::app::Key::Char(c));
            }
            // Wide enough for the whole of `HINTS`: at sixty columns `quit` no
            // longer fits either, and the assertion below would pass whether or
            // not the prompt had replaced anything.
            let lines = render(&a, 70, 8);
            assert!(
                lines[7].contains(expected),
                "prompt should be on the bottom row: {:?}",
                lines[7]
            );
            assert!(
                !lines[7].contains("quit"),
                "hints should be replaced, not appended"
            );
            assert!(lines[..7].iter().any(|l| l.contains("dotfiles")));
        }
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
        assert!(lines[..5].iter().any(|l| l.contains("dotfiles")));
        assert!(!lines[..5].iter().any(|l| l.contains("api-server")));
    }

    /// `HINTS` no longer fits every terminal, so which entry goes first is a
    /// decision rather than an accident. `q quit` is the one to lose: it is what
    /// every user of a full-screen program tries unprompted, and `Ctrl-c` quits
    /// as well — whereas `␣ order` is the entry the row grew to carry.
    #[test]
    fn the_hint_row_gives_up_quit_first_when_it_does_not_fit() {
        let lines = render(&app(&["one"]), 62, 6);
        let row = &lines[5];
        assert!(row.contains("order"), "the new entry was cut: {row:?}");
        assert!(row.contains("attach"), "{row:?}");
        assert!(
            !row.contains("quit"),
            "something else was cut first: {row:?}"
        );
        assert!(row.width() <= 62, "the row overflowed: {row:?}");
    }

    /// The README's picture of the picker reproduces this row byte for byte, and
    /// nothing else ties the two together — so without this the art quietly
    /// describes a program that no longer exists. `keys.rs` pins its own README
    /// table the same way.
    #[test]
    fn the_readme_shows_the_hint_row_the_picker_actually_prints() {
        assert!(
            include_str!("../../README.md").contains(HINTS),
            "the README's picker art is stale — it should carry this row \
             verbatim:\n{HINTS}"
        );
    }

    #[test]
    fn a_narrow_terminal_truncates_the_hints_rather_than_wrapping() {
        let lines = render(&app(&["one"]), 20, 5);
        assert_eq!(lines.len(), 5);
        // Row 3 must stay part of the list area, not spillover from the hints.
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
            lines.iter().any(|l| l.contains("▸ 30  session-29")),
            "the selection scrolled out of view: {lines:#?}"
        );
        // Thirty sessions means two-digit numbers. Whatever is on screen, the
        // names line up in one column: the marker and the number column are
        // both fixed width, so nothing shifts as the selection moves.
        let name_columns: Vec<usize> = lines
            .iter()
            .filter(|l| l.contains("session-"))
            .map(|l| l[..l.find("session-").expect("a name")].width())
            .collect();
        assert_eq!(name_columns.len(), 9, "expected a full window: {lines:#?}");
        assert!(
            name_columns.windows(2).all(|w| w[0] == w[1]),
            "names are not in one column: {lines:#?}"
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
        for &(w, h) in test_support::TINY_SIZES {
            let _ = render(&app(&["one", "two"]), w.max(1), h.max(1));
            let _ = render(&app(&[]), w.max(1), h.max(1));
        }
    }

    /// The picker must never emit an SGR colour, so it inherits the terminal's
    /// palette and background and is correct under NO_COLOR without needing to
    /// test for it.
    #[test]
    fn nothing_sets_a_colour() {
        let mut a = app(&["one", "two", "three"]);
        a.on_key(super::super::app::Key::Char('j'));
        test_support::assert_no_colour(50, 8, |f| draw(f, &a));

        let b = reordering(&["one", "two", "three"], 1);
        test_support::assert_no_colour(50, 8, |f| draw(f, &b));
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
