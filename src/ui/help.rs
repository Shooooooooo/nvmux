//! The key-binding help, shown by `?` on the picker and `<prefix> ?` in a
//! session.
//!
//! Over a session it floats in a [`Panel`] (see `crate::pty::panel`) unless
//! `[style] float` is off or there is no room ([`Floating::area`]); its hint
//! row stays on the screen's last row.
//!
//! Command rows come from the [`Keymap`] [`keys::Prefix::feed`] runs on, so
//! the screen cannot list a key that does nothing; a test pins the other
//! rows to the machine.
//!
//! Only named keys close it. `<prefix>` arrives as `Key::Other` (`translate`
//! maps every control chord but `Ctrl-c`, `Ctrl-n`, `Ctrl-p` to it), so if
//! any key closed it, typing "Ctrl-Space d" would close on the prefix and
//! send a bare `d` to the session. Nothing typed here is forwarded.

use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyEvent, KeyEventKind, MouseEvent};
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use super::app::Key;
use super::draw;
use super::float::{self, Did, Floating};
use crate::error::Result;
use crate::keys::{self, Keymap};

/// `q`, `Enter`, `?` and `Ctrl-c` also close, unadvertised.
const HINTS: &str = "esc back";

/// Columns between the key column and the description.
const GAP: usize = 3;

/// Minimum session left visible on each side of the floating help.
const SESSION_COLS: u16 = 2;
const SESSION_ROWS: u16 = 1;

#[derive(Debug)]
struct Row {
    keys: String,
    what: String,
}

/// The command rows on `keymap`'s keys, then the fixed rules. `prefix` labels
/// the prefix key (see [`keys::prefix_label`]).
fn rows(prefix: &str, keymap: &Keymap) -> Vec<Row> {
    let mut rows: Vec<Row> = keymap
        .bindings()
        .map(|b| Row {
            keys: format!("{prefix} {}", keys::key_label(b.key)),
            what: b.help.to_string(),
        })
        .collect();
    rows.push(Row {
        // Not `1-9`: `Prefix::feed` reads a number, so `12` reaches the
        // twelfth session.
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

/// Named keys only (see the module docs). `q` and `Ctrl-c` close rather than
/// quit, so reading the help never exits a live session.
fn closes(key: Key) -> bool {
    matches!(
        key,
        Key::Char('q') | Key::Char('?') | Key::Esc | Key::Enter | Key::CtrlC
    )
}

/// Show the help full screen until dismissed.
pub fn run() -> Result<()> {
    // Reached from a session that just dissolved out, so dissolve in.
    super::owning(|terminal| run_on(terminal, true))
}

/// Show the help on a terminal the caller owns (the picker's `?`), dissolving
/// in and out if `animate`.
pub(super) fn run_on(terminal: &mut ratatui::DefaultTerminal, animate: bool) -> Result<()> {
    let settings = crate::config::get().keys;
    let rows = rows(&keys::prefix_label(settings.prefix), &settings.bind);
    if animate {
        crate::fade::fade_in(terminal, |f| draw(f, &rows))?;
    }
    loop {
        terminal.draw(|f| {
            draw(f, &rows);
            crate::theme::paint(f.buffer_mut());
        })?;

        let Some(key) = super::poll_key()? else {
            continue;
        };

        if closes(key) {
            // So the resumed session takes over from the background, not
            // from the key list.
            if animate {
                crate::fade::fade_out(terminal, |f| draw(f, &rows))?;
            }
            return Ok(());
        }
    }
}

fn draw(frame: &mut Frame, rows: &[Row]) {
    let area = frame.area();
    draw_on(frame.buffer_mut(), area, rows);
}

/// Draw on `area` (the terminal, or the panel's inside) in the usual screen
/// layout (see [`draw::screen`]). The panel keeps all but the hint row
/// ([`float::lay`]).
fn draw_on(buf: &mut Buffer, area: Rect, rows: &[Row]) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let (body, hint) = draw::split_hint_row(area);
    draw_table(buf, rows, body);
    draw::hint_row_on(buf, hint, HINTS, true);
}

/// The table's (width, height).
fn table_size(rows: &[Row]) -> (u16, u16) {
    let key_width = rows.iter().map(|r| r.keys.width()).max().unwrap_or(0);
    let what_width = rows.iter().map(|r| r.what.width()).max().unwrap_or(0);
    ((key_width + GAP + what_width) as u16, rows.len() as u16)
}

/// Keys are padded by display width (not `{:<w$}`) so non-ASCII labels align.
/// Rows are truncated, never wrapped, to keep the hint row in place.
fn draw_table(buf: &mut Buffer, rows: &[Row], area: Rect) {
    if area.height == 0 || area.width == 0 || rows.is_empty() {
        return;
    }
    let key_width = rows.iter().map(|r| r.keys.width()).max().unwrap_or(0);
    let (width, height) = table_size(rows);
    let block = draw::centre(area, width, height);

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
    Paragraph::new(lines).render(block, buf);
}

/// The help floating over a session, sized to the table plus padding and
/// margins ([`float::inside`]).
#[derive(Debug)]
pub struct Panel {
    rows: Vec<Row>,
}

impl Panel {
    pub fn open() -> Self {
        let settings = crate::config::get().keys;
        Self {
            rows: rows(&keys::prefix_label(settings.prefix), &settings.bind),
        }
    }

    fn size(&self) -> (u16, u16) {
        let (width, height) = table_size(&self.rows);
        (width + 4, height + 3)
    }
}

impl Floating for Panel {
    /// `None` (full screen) without [`SESSION_COLS`] and [`SESSION_ROWS`] of
    /// session around it.
    fn area(&self, screen: Rect) -> Option<Rect> {
        let (room, _) = float::split(screen);
        let (width, height) = self.size();
        if room.width < width + 2 * SESSION_COLS || room.height < height + 2 * SESSION_ROWS {
            return None;
        }
        Some(Rect::new(
            room.x + (room.width - width) / 2,
            room.y + (room.height - height) / 2,
            width,
            height,
        ))
    }

    /// See [`closes`]; releases are ignored.
    fn on_key(&mut self, key: KeyEvent, _now: Instant) -> Did {
        if key.kind != KeyEventKind::Release && closes(super::translate(key)) {
            Did::Close
        } else {
            Did::Stay
        }
    }

    /// Nothing to click. A press on the session closes it, which the relay
    /// handles.
    fn on_mouse(&mut self, _mouse: MouseEvent, _now: Instant) -> Did {
        Did::Stay
    }

    fn wake_at(&self) -> Option<Instant> {
        None
    }

    fn tick(&mut self, _now: Instant) -> bool {
        false
    }

    fn moving(&self) -> bool {
        false
    }

    fn draw(&mut self, area: Rect) -> Buffer {
        let mut buf = Buffer::empty(area);
        let inside = float::inside(area);
        let mut drawn = Buffer::empty(inside);
        draw_on(&mut drawn, inside, &self.rows);
        float::lay(&mut buf, inside, &drawn);
        crate::theme::paint(&mut buf);
        buf
    }

    fn draw_hints(&self, row: Rect) -> Buffer {
        let mut buf = Buffer::empty(row);
        draw::hint_row_on(&mut buf, row, HINTS, true);
        crate::theme::paint(&mut buf);
        buf
    }

    fn close(&self) {}
}

#[cfg(test)]
mod tests {
    use super::super::test_support;
    use super::*;
    use crate::keys::{Prefix, PREFIX, PREFIX_LABEL};
    use ratatui::style::Modifier;

    /// Wide enough that centring is measured, not clamping.
    const WIDE: u16 = 70;

    /// [`super::rows`] on the default keymap.
    fn rows(prefix: &str) -> Vec<Row> {
        super::rows(prefix, &Keymap::default())
    }

    fn render(w: u16, h: u16) -> Vec<String> {
        let rows = rows(PREFIX_LABEL);
        test_support::render(w, h, |f| draw(f, &rows))
    }

    /// The line starting with a row's keys. Not `contains`: "send a literal
    /// Ctrl-Space to Neovim" contains `Ctrl-Space t`.
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

    /// Against the prefix machine itself, not the table both read, on default
    /// and remapped keys. `highest` is 0 so digits resolve at once.
    #[test]
    fn every_row_matches_what_the_prefix_machine_does() {
        let mut moved = Keymap::default();
        for (name, key) in [("picker", b'D'), ("detach", b' '), ("go", b'g')] {
            assert!(moved.bind(name, key), "no command is called {name}");
        }
        for keymap in [Keymap::default(), moved] {
            let rows = super::rows(PREFIX_LABEL, &keymap);
            for b in 0u8..=255 {
                let acts = Prefix::with_keys(0, PREFIX, keymap)
                    .feed(&[PREFIX, b])
                    .iter()
                    .any(|s| matches!(s, keys::Step::Act(_)));
                // Equality, not `starts_with`: `o` would match the
                // `<prefix> other` row. Digits are looked up as the range row.
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
    }

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
            // No session number starts with 0.
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
        // Two rows of slack, so centring is measured, not clamping.
        let rows = keys::BINDINGS.len() + 3;
        let height = rows as u16 + 3;
        let body = usize::from(height) - 1;
        let lines = render(WIDE, height);
        assert_eq!(lines.len(), usize::from(height));

        assert_eq!(lines[body].trim(), HINTS, "hints should be on the last row");

        let occupied: Vec<usize> = lines[..body]
            .iter()
            .enumerate()
            .filter(|(_, l)| !l.is_empty())
            .map(|(i, _)| i)
            .collect();
        // Centring puts an odd row of slack at the bottom.
        let top = (body - rows) / 2;
        assert_eq!(
            occupied,
            (top..top + rows).collect::<Vec<_>>(),
            "table is not vertically centred: {lines:#?}"
        );

        // Only the widest row defines the block; shorter ones have slack on
        // the right.
        let row = lines[..body]
            .iter()
            .max_by_key(|l| l.width())
            .expect("a table row");
        let (left, right) = test_support::padding(row, WIDE as usize);
        assert!(
            left.abs_diff(right) <= 2,
            "not horizontally centred: {left} left, {right} right, row {row:?}"
        );

        test_support::assert_no_borders(&lines);
    }

    #[test]
    fn the_hint_row_is_dim_and_the_table_is_not() {
        let rows = rows(PREFIX_LABEL);
        let buf = test_support::buffer(WIDE, 11, |f| draw(f, &rows));
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

    #[test]
    fn nothing_sets_a_colour() {
        let rows = rows(PREFIX_LABEL);
        test_support::assert_no_colour(WIDE, 11, |f| draw(f, &rows));
    }

    /// A panel on the default keymap, independent of the live config.
    fn panel() -> Panel {
        Panel {
            rows: rows(PREFIX_LABEL),
        }
    }

    fn rows_of(buf: &Buffer) -> Vec<String> {
        let area = buf.area;
        (area.y..area.y + area.height)
            .map(|y| {
                (area.x..area.x + area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    /// The panel goes in the room above the hint row: all but the last row.
    #[test]
    fn the_panel_is_the_tables_size_in_the_middle() {
        let panel = panel();
        let (w, h) = table_size(&panel.rows);
        let (pw, ph) = (w + 4, h + 3);
        let screen = |cols, rows| Rect::new(0, 0, cols, rows);
        assert_eq!(
            panel.area(screen(120, 36)),
            Some(Rect::new((120 - pw) / 2, (35 - ph) / 2, pw, ph))
        );
        let on_80 = panel.area(screen(80, 24)).expect("floats on 80x24");
        assert!(on_80.x >= SESSION_COLS && on_80.right() <= 80 - SESSION_COLS);
        assert!(
            on_80.y >= SESSION_ROWS && on_80.bottom() <= 23 - SESSION_ROWS,
            "a row of the session over the hint row: {on_80:?}"
        );
        let fits = (pw + 2 * SESSION_COLS, ph + 2 * SESSION_ROWS + 1);
        assert!(panel.area(screen(fits.0, fits.1)).is_some(), "just fits");
        assert_eq!(panel.area(screen(fits.0 - 1, 40)), None, "too narrow");
        assert_eq!(panel.area(screen(200, fits.1 - 1)), None, "too short");
    }

    /// The help does not scroll, so no row may be cut.
    #[test]
    fn the_panel_draws_every_row_whole_and_its_hint_row_is_the_screens() {
        let mut panel = panel();
        let screen = Rect::new(0, 0, 80, 24);
        let area = panel.area(screen).expect("floats");
        let buf = panel.draw(area);
        assert_eq!(buf.area, area);
        let lines = rows_of(&buf);
        let last = lines.len() - 1;
        assert!(lines[0].trim().is_empty(), "a clear row over the table");
        for row in &panel.rows {
            let line = line_for(&lines, row);
            assert!(line.contains(&row.what), "{row:?} cut: {line:?}");
        }
        assert!(lines[last - 1].trim().is_empty(), "a clear row under it");
        assert!(lines[last].trim().is_empty(), "the last row clear");
        assert!(!lines.iter().any(|l| l.contains(HINTS)), "{lines:#?}");
        for y in area.y..area.y + area.height {
            assert_eq!(buf[(area.x, y)].symbol(), " ", "the left margin");
            assert_eq!(buf[(area.right() - 1, y)].symbol(), " ", "the right");
        }

        let (_, row) = float::split(screen);
        let hints = panel.draw_hints(row);
        let rows = rows(PREFIX_LABEL);
        let whole = test_support::buffer(80, 24, |f| draw(f, &rows));
        for x in 0..80 {
            assert_eq!(hints[(x, 23)], whole[(x, 23)], "column {x}");
        }
        assert_eq!(rows_of(&hints)[0].trim(), HINTS);
    }

    #[test]
    fn the_panel_closes_on_the_screens_keys_only() {
        use ratatui::crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};
        let now = Instant::now();
        let mut panel = panel();
        let press = |code, modifiers| KeyEvent::new(code, modifiers);
        for code in [
            KeyCode::Esc,
            KeyCode::Char('q'),
            KeyCode::Char('?'),
            KeyCode::Enter,
        ] {
            assert_eq!(
                panel.on_key(press(code, KeyModifiers::NONE), now),
                Did::Close,
                "{code:?}"
            );
        }
        let ctrl_c = press(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(panel.on_key(ctrl_c, now), Did::Close);
        let prefix = press(KeyCode::Char(' '), KeyModifiers::CONTROL);
        assert_eq!(panel.on_key(prefix, now), Did::Stay, "the prefix");
        let d = press(KeyCode::Char('d'), KeyModifiers::NONE);
        assert_eq!(panel.on_key(d, now), Did::Stay, "a command's key");
        let up = KeyEvent::new_with_kind(KeyCode::Esc, KeyModifiers::NONE, KeyEventKind::Release);
        assert_eq!(panel.on_key(up, now), Did::Stay, "a release");
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 10,
            row: 10,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(panel.on_mouse(click, now), Did::Stay);
        assert!(!panel.moving() && panel.wake_at().is_none() && !panel.tick(now));
    }
}
