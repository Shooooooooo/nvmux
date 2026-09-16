//! The key-binding help — a third screen, shown by `<prefix> ?`.
//!
//! A whole screen rather than a popup for a harder reason than taste: what a
//! popup would cover is Neovim's screen, and [`crate::pty`] never writes into
//! the child's output. So this takes the terminal the way the prompt does,
//! draws, and hands the same client back to repaint on resume.
//!
//! Every command row comes from [`keys::BINDINGS`], the same table
//! [`keys::Prefix::feed`] runs on, so this screen cannot claim something the
//! machine does not do. The rows that are not commands — a digit selects a
//! session, `<prefix> <prefix>` is a literal, anything else is replayed — are the
//! machine's other branches, spelled here and pinned to it by a test.
//!
//! # Why only named keys close it
//!
//! `<prefix>` arrives here as `Key::Other`, because `translate` turns every
//! control chord but `Ctrl-c`, `Ctrl-n` and `Ctrl-p` into nothing. If any key
//! closed the help, someone who read "Ctrl-Space d" and typed it would close
//! the screen on the `Ctrl-Space` and send a bare `d` into normal mode. So the
//! space bar, `d`, `c` and `Other` do nothing, and only `Esc`, `q`, `Enter`,
//! `?` and `Ctrl-c` close it. Nothing typed on this screen is ever forwarded.

use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use super::app::Key;
use super::draw;
use crate::error::Result;
use crate::keys;

/// Key then action, the picker's hint grammar. `q`, `Enter`, `?` and `Ctrl-c`
/// also close, unadvertised.
const HINTS: &str = "esc back";

/// Columns between the key column and the description.
const GAP: usize = 3;

/// One line of the table.
#[derive(Debug)]
struct Row {
    keys: String,
    what: String,
}

/// The command rows from [`keys::BINDINGS`], then the rules that are not
/// commands: digits select a session, a doubled prefix is a literal, and
/// anything else is replayed.
///
/// `prefix` is the label to spell the prefix with — [`keys::PREFIX_LABEL`] by
/// default, or whatever a remapped prefix reads as (see
/// [`keys::prefix_label`]) — so every row, and the two descriptions that name
/// the prefix, follow the key the user actually set.
fn rows(prefix: &str) -> Vec<Row> {
    let mut rows: Vec<Row> = keys::BINDINGS
        .iter()
        .map(|b| Row {
            keys: format!("{prefix} {}", keys::key_label(b.key)),
            what: b.help.to_string(),
        })
        .collect();
    rows.push(Row {
        // Not `1-9`: `Prefix::feed` reads a number, not a digit, so `12` reaches
        // the twelfth session.
        keys: format!("{prefix} 1-n"),
        what: "attach to the session with that number".to_string(),
    });
    rows.push(Row {
        keys: format!("{prefix} {prefix}"),
        what: format!("send a literal {prefix} to Neovim"),
    });
    rows.push(Row {
        keys: format!("{prefix} other"),
        what: format!("send {prefix} and that key to Neovim"),
    });
    rows
}

/// Named keys only. The space bar, `d` and `c` — and therefore the prefix,
/// which arrives as `Key::Other` — are deliberately not here: a chord typed
/// while the help is open must do nothing, not close the help and forward its
/// second key.
///
/// `q` and `Ctrl-c` close like esc rather than quitting: quitting here would
/// tear the user out of a live session they only meant to read a key list in.
fn closes(key: Key) -> bool {
    matches!(
        key,
        Key::Char('q') | Key::Char('?') | Key::Esc | Key::Enter | Key::CtrlC
    )
}

/// Show the bindings until the user dismisses them, on its own terminal, handing
/// the session back untouched afterwards.
pub fn run() -> Result<()> {
    // Reached from a session that has just dissolved out, so dissolve in.
    super::owning(|terminal| run_on(terminal, crate::fade::excursions()))
}

/// Show the bindings on a terminal the caller already owns — how the picker
/// answers `?`. `animate` is whether to dissolve in on the way in and out on
/// the way out; from the picker the screen is already up, so it does not.
pub(super) fn run_on(terminal: &mut ratatui::DefaultTerminal, animate: bool) -> Result<()> {
    let label = keys::prefix_label(crate::config::get().keys.prefix);
    let rows = rows(&label);
    if animate {
        crate::fade::fade_in(terminal, |f| draw(f, &rows))?;
    }
    loop {
        terminal.draw(|f| draw(f, &rows))?;

        let Some(key) = super::poll_key()? else {
            continue;
        };

        if closes(key) {
            // Dissolve out, so the resumed session takes over from the
            // background rather than from the key list.
            if animate {
                crate::fade::fade_out(terminal, |f| draw(f, &rows))?;
            }
            return Ok(());
        }
    }
}

fn draw(frame: &mut Frame, rows: &[Row]) {
    let area = frame.area();
    if area.height == 0 || area.width == 0 {
        return;
    }

    let (body, bottom) = draw::split_hint_row(area);

    draw_table(frame, rows, body);
    draw::draw_hint_row(frame, bottom, HINTS, true);
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

#[cfg(test)]
mod tests {
    use super::super::test_support;
    use super::*;
    use crate::keys::{Prefix, PREFIX, PREFIX_LABEL};
    use ratatui::backend::TestBackend;
    use ratatui::style::Modifier;
    use ratatui::Terminal;

    /// Wide enough that the whole table fits with slack on both sides, so the
    /// centring is what is being measured rather than the clamp.
    const WIDE: u16 = 70;

    fn render(w: u16, h: u16) -> Vec<String> {
        let rows = rows(PREFIX_LABEL);
        test_support::render(w, h, |f| draw(f, &rows))
    }

    /// The line a row was drawn on. Matched on the *start* of the line rather
    /// than anywhere in it: "send a literal Ctrl-Space to Neovim" contains the
    /// text `Ctrl-Space t`, so `contains` would find the picker row twice.
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
            assert!(closes(key), "{key:?} should close");
        }
    }

    /// `<prefix>` arrives here as `Key::Other`, and its command keys as
    /// themselves; if any of them closed the help, a chord typed from this
    /// screen would be half-forwarded.
    #[test]
    fn the_prefix_and_its_command_keys_do_nothing_here() {
        for key in [
            Key::Char(' '),
            Key::Char('d'),
            Key::Char('c'),
            Key::Char('n'),
            Key::Char('p'),
            Key::Up,
            Key::Backspace,
            Key::Other,
        ] {
            assert!(!closes(key), "{key:?} must be ignored");
        }
    }

    /// The rendered rows are checked against what the machine actually does,
    /// not against the table both of them read.
    ///
    /// `highest` is 0 so every digit resolves on the spot; what is being checked
    /// is which bytes are commands at all, not how long one waits.
    #[test]
    fn every_row_matches_what_the_prefix_machine_does() {
        let rows = rows(PREFIX_LABEL);
        for b in 0u8..=255 {
            let acts = Prefix::new(0)
                .feed(&[PREFIX, b])
                .iter()
                .any(|s| matches!(s, keys::Step::Act(_)));
            // Equality, not `starts_with`: `o` would otherwise match the
            // `<prefix> other` row. Digits are the one range row, so they are
            // looked up as the range rather than as themselves.
            let listed = if b.is_ascii_digit() {
                acts && rows.iter().any(|r| r.keys == format!("{PREFIX_LABEL} 1-n"))
            } else {
                rows.iter()
                    .any(|r| r.keys == format!("{PREFIX_LABEL} {}", keys::key_label(b)))
            };
            assert_eq!(
                acts, listed,
                "byte {b:#04x}: the machine acts = {acts}, the help lists it = {listed}"
            );
        }
    }

    /// The range row is a promise about nine specific bytes; check them.
    #[test]
    fn the_digit_row_covers_exactly_one_to_nine() {
        assert!(
            rows(PREFIX_LABEL)
                .iter()
                .any(|r| r.keys == format!("{PREFIX_LABEL} 1-n")),
            "the digit row is missing"
        );
        for b in b'0'..=b'9' {
            let acts = Prefix::new(0)
                .feed(&[PREFIX, b])
                .iter()
                .any(|s| matches!(s, keys::Step::Act(_)));
            // Zero is excluded on purpose: no session number begins with one.
            assert_eq!(acts, b != b'0', "byte {:?}", b as char);
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
            .map(|b| format!("{PREFIX_LABEL} {}", keys::key_label(b.key)))
            .collect();
        assert_eq!(body.len(), want.len() + 3, "body rows: {body:#?}");
        for (line, key) in body.iter().zip(&want) {
            assert!(
                line.starts_with(key.as_str()),
                "expected {key:?} at the start of {line:?}"
            );
        }
        let digits = body[want.len()];
        assert!(
            digits.starts_with(&format!("{PREFIX_LABEL} 1-n")),
            "got {digits:?}"
        );
        assert!(digits.contains("number"), "got {digits:?}");
        let literal = body[want.len() + 1];
        assert!(
            literal.starts_with(&format!("{PREFIX_LABEL} {PREFIX_LABEL}")),
            "got {literal:?}"
        );
        assert!(literal.contains("literal"), "got {literal:?}");
        let other = body[want.len() + 2];
        assert!(
            other.starts_with(&format!("{PREFIX_LABEL} other")),
            "got {other:?}"
        );
        assert!(other.contains("that key"), "got {other:?}");
    }

    /// A remapped prefix relabels every row *and* the two descriptions that name
    /// the prefix — not just the key column — so the screen never claims a key the
    /// user did not set.
    #[test]
    fn a_remapped_prefix_labels_every_row_and_the_literal_text() {
        let rows = rows("Ctrl-a");
        for r in &rows {
            assert!(
                r.keys.starts_with("Ctrl-a"),
                "row {:?} kept the old prefix",
                r.keys
            );
        }
        let literal = rows
            .iter()
            .find(|r| r.keys == "Ctrl-a Ctrl-a")
            .expect("the literal row");
        assert_eq!(&*literal.what, "send a literal Ctrl-a to Neovim");
        let other = rows
            .iter()
            .find(|r| r.keys == "Ctrl-a other")
            .expect("the other row");
        assert_eq!(&*other.what, "send Ctrl-a and that key to Neovim");
    }

    #[test]
    fn descriptions_line_up_in_one_column() {
        let lines = render(80, 24);
        let cols: Vec<usize> = rows(PREFIX_LABEL)
            .iter()
            .map(|r| {
                let line = line_for(&lines, r);
                let byte = line.find(&*r.what).expect("description present");
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
        let lines = render(WIDE, 11);
        assert_eq!(lines.len(), 11);

        assert_eq!(lines[10].trim(), HINTS, "hints should be on the last row");

        let occupied: Vec<usize> = lines[..10]
            .iter()
            .enumerate()
            .filter(|(_, l)| !l.is_empty())
            .map(|(i, _)| i)
            .collect();
        // Derived rather than listed, so adding a binding does not turn a
        // centring test into a counting one: the table is every row of
        // `BINDINGS` plus the three fixed rules, and centring puts any odd row
        // of slack at the bottom.
        let rows = keys::BINDINGS.len() + 3;
        let top = (10 - rows) / 2;
        assert_eq!(
            occupied,
            (top..top + rows).collect::<Vec<_>>(),
            "table is not vertically centred: {lines:#?}"
        );

        // The widest row is the one that defines the block; a shorter one has
        // slack on the right by construction.
        let row = lines[..10]
            .iter()
            .max_by_key(|l| l.width())
            .expect("a table row");
        let left = row.len() - row.trim_start().len();
        let right = WIDE as usize - row.width();
        assert!(
            left.abs_diff(right) <= 2,
            "not horizontally centred: {left} left, {right} right, row {row:?}"
        );

        test_support::assert_no_borders(&lines);
    }

    #[test]
    fn the_hint_row_is_dim_and_the_table_is_not() {
        let rows = rows(PREFIX_LABEL);
        let mut terminal = Terminal::new(TestBackend::new(WIDE, 11)).expect("terminal");
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
            let last = &lines[h as usize - 1];
            assert_eq!(last.trim(), HINTS, "{w}x{h}: {lines:#?}");
            assert!(
                !lines[..h as usize - 1].iter().any(|l| l.contains(HINTS)),
                "{w}x{h}: the hint wrapped upward: {lines:#?}"
            );
        }
        let lines = render(20, 5);
        let body: Vec<&String> = lines[..4].iter().filter(|l| !l.is_empty()).collect();
        assert_eq!(body.len(), 4, "{lines:#?}");
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        for &(w, h) in test_support::TINY_SIZES {
            let _ = render(w.max(1), h.max(1));
        }
    }

    /// No SGR colour, so the screen is correct under NO_COLOR by construction.
    #[test]
    fn nothing_sets_a_colour() {
        let rows = rows(PREFIX_LABEL);
        test_support::assert_no_colour(WIDE, 11, |f| draw(f, &rows));
    }
}
