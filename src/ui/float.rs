//! Screens drawn in a panel over a session (see `crate::pty::panel`): the
//! settings ([`super::settings::Panel`]), the help ([`super::help::Panel`])
//! and the picker ([`super::picker::Panel`]).
//!
//! The relay reads the terminal while one is up, the screen being drawn over a
//! session that has it, and hands each its keys and the mouse; it asks where
//! the screen goes, draws it into a buffer of that size, and lays the buffer
//! over the session dimmed round it. What floats keeps its own rules, every
//! one, as it has them full screen: what changes is where it is drawn.
//!
//! All but its hint row. That does not float: it is the last row of the
//! screen, the whole width of it, as full screen ([`Floating::draw_hints`]),
//! and the panel goes in the room above it ([`split`]). So turning
//! `[style] float` moves the screen and leaves its hint row as it was, where
//! it was, saying what it said.

use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyEvent, MouseEvent};
use ratatui::layout::Rect;

/// What the relay should do after a key or mouse event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Did {
    Stay,
    Close,
}

/// Where to go after a floating screen closes, if not back to the session
/// (set by the picker's keys).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leave {
    /// `Enter` on a row.
    Switch(crate::ids::Id),
    /// `c`: new session placed after the given one (the cursor row), `None`
    /// if there are no rows. The picker returns if the prompt is cancelled.
    New(Option<crate::ids::Id>),
    /// `q`: exit nvmux, leaving sessions running.
    Quit,
    /// `r`. The picker returns once the prompt is answered.
    Rename(crate::ids::Id),
    /// `?`. Closing it returns to the picker.
    Help,
    /// `,`. Closing it returns to the picker.
    Settings,
}

pub trait Floating {
    /// Where it floats on `screen`, or `None` where there is no room for it
    /// and the session round it: the relay then shows it full screen. In the
    /// room above the hint row ([`split`]).
    fn area(&self, screen: Rect) -> Option<Rect>;

    fn on_key(&mut self, key: KeyEvent, now: Instant) -> Did;

    /// A mouse report inside the screen or out of it; a press on the session
    /// — anywhere but the panel and the hint row — is the relay's, which
    /// takes the screen down.
    fn on_mouse(&mut self, mouse: MouseEvent, now: Instant) -> Did;

    /// When it next changes on its own, if ever.
    fn wake_at(&self) -> Option<Instant>;

    /// Advance to `now`; returns whether anything changed.
    fn tick(&mut self, now: Instant) -> bool;

    /// Whether it is animating, so frames run at the frame rate.
    fn moving(&self) -> bool;

    /// The screen drawn into `area`, a buffer of that area: all of it but its
    /// hint row (see [`inside`]).
    fn draw(&mut self, area: Rect) -> Buffer;

    /// Its hint row drawn into `row`, a buffer of that row: the last row of
    /// the screen, the whole width of it ([`split`]), saying what the screen
    /// full screen says on its last row on a terminal as wide — and blank
    /// either side of that, which clears the session's own last row from
    /// under it.
    fn draw_hints(&self, row: Rect) -> Buffer;

    /// Taken down.
    fn close(&self);

    /// Reshown after being set aside, e.g. the picker after help or settings.
    fn back(&mut self) {}

    fn leave(&self) -> Option<Leave> {
        None
    }

    /// Whether it wants to close without a key or mouse event: the picker
    /// once a half-typed number times out, or once the gap `c` opened is up.
    fn closing(&self) -> bool {
        false
    }
}

/// The room on `screen` that a screen floating over a session has its panel
/// in, and its hint row under that: the hint row on the last row, the whole
/// width of the screen, as it is full screen (see
/// [`super::draw::split_hint_row`]), and the room all of the screen above it.
/// Both empty on a screen with no rows.
pub fn split(screen: Rect) -> (Rect, Rect) {
    if screen.height == 0 {
        return (screen, screen);
    }
    super::draw::split_hint_row(screen)
}

/// Where a screen goes in the panel it floats in: all of it but a column
/// either side, which keep its text off the session's, laid out as on a
/// terminal of that size — but for the last row, where the screen has its
/// hint row full screen. That goes along the bottom of the screen
/// ([`Floating::draw_hints`]), and here the row is left blank, keeping the
/// screen's text off the session's under the panel too ([`lay`]).
pub(super) fn inside(panel: Rect) -> Rect {
    Rect::new(
        panel.x + 1,
        panel.y,
        panel.width.saturating_sub(2),
        panel.height,
    )
}

/// What a screen drew of itself on a terminal the size of the panel's inside
/// ([`inside`]) — `drawn`, a buffer of that size — laid into the panel's
/// `buf` there: all of it but its last row, the hint row's, which is left
/// blank.
pub(super) fn lay(buf: &mut Buffer, inside: Rect, drawn: &Buffer) {
    let from = drawn.area;
    let height = inside.height.min(from.height).saturating_sub(1);
    for y in 0..height {
        for x in 0..inside.width.min(from.width) {
            buf[(inside.x + x, inside.y + y)] = drawn[(from.x + x, from.y + y)].clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A column off either side, and every row: the last is the hint row's.
    #[test]
    fn the_inside_leaves_a_column_either_side() {
        assert_eq!(inside(Rect::new(8, 3, 64, 18)), Rect::new(9, 3, 62, 18));
        assert_eq!(inside(Rect::new(0, 0, 1, 1)), Rect::new(1, 0, 0, 1));
    }

    /// The hint row is the screen's last row, the whole width of it, and the
    /// room for the panel all of the screen above it.
    #[test]
    fn the_hint_row_is_the_last_row_and_the_room_the_rest() {
        assert_eq!(
            split(Rect::new(0, 0, 80, 24)),
            (Rect::new(0, 0, 80, 23), Rect::new(0, 23, 80, 1))
        );
        assert_eq!(
            split(Rect::new(0, 0, 80, 1)),
            (Rect::new(0, 0, 80, 0), Rect::new(0, 0, 80, 1))
        );
        let none = Rect::new(0, 0, 80, 0);
        assert_eq!(split(none), (none, none));
    }

    /// What a screen drew goes where the inside is, every row of it but the
    /// last, which stays blank.
    #[test]
    fn a_screen_is_laid_in_but_for_its_hint_row() {
        let inside = inside(Rect::new(8, 3, 6, 3));
        let mut drawn = Buffer::empty(Rect::new(0, 0, inside.width, inside.height));
        for (y, text) in ["body", "more", "hint"].into_iter().enumerate() {
            drawn.set_string(0, y as u16, text, ratatui::style::Style::default());
        }
        let mut buf = Buffer::empty(Rect::new(8, 3, 6, 3));
        lay(&mut buf, inside, &drawn);
        let row = |y| -> String { (8..14).map(|x| buf[(x, y)].symbol()).collect() };
        assert_eq!(row(3), " body ");
        assert_eq!(row(4), " more ");
        assert_eq!(row(5), "      ", "the hint row's left blank");
    }
}
