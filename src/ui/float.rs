//! What floats over a session in a panel the relay draws (see
//! `crate::pty::panel`): the settings ([`super::settings::Panel`]), the
//! help ([`super::help::Panel`]) and the picker ([`super::picker::Panel`]).
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

/// What a key or a gesture on a screen floating over a session leaves for the
/// relay to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Did {
    Stay,
    /// The key that closes the screen: take it down.
    Close,
}

/// Where a screen floating over a session goes once it is taken down, when
/// that is not back to the session it floats over: what the picker's keys
/// lead to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Leave {
    /// To another session: `Enter` on its row.
    Switch(crate::ids::Id),
    /// To set up a new session, which goes after the session named, the row
    /// the cursor was on, as the picker full screen puts one: `c`. `None`
    /// where there are no rows. The prompt comes up in the picker's place,
    /// which comes back where it is cancelled.
    New(Option<crate::ids::Id>),
    /// Out of nvmux, every session left running: `q`.
    Quit,
    /// To rename this session: `r`. The prompt comes up in the picker's place,
    /// which comes back once it has been answered.
    Rename(crate::ids::Id),
    /// The help, over the session in its place: `?`. Taken down, it goes back
    /// to the picker, as the help full screen does.
    Help,
    /// The settings, over the session in its place: `,`. Taken down, they go
    /// back to the picker, as the settings full screen do.
    Settings,
}

/// A screen that floats over a session.
pub trait Floating {
    /// Where it floats on `screen`, or `None` where there is no room for it
    /// and the session round it: the relay then shows it full screen. In the
    /// room above the hint row ([`split`]).
    fn area(&self, screen: Rect) -> Option<Rect>;

    /// A key, as the terminal reported it.
    fn on_key(&mut self, key: KeyEvent, now: Instant) -> Did;

    /// A mouse report inside the screen or out of it; a press on the session
    /// — anywhere but the panel and the hint row — is the relay's, which
    /// takes the screen down.
    fn on_mouse(&mut self, mouse: MouseEvent, now: Instant) -> Did;

    /// When something on it changes on its own, if anything is to.
    fn wake_at(&self) -> Option<Instant>;

    /// Moved on to `now`: whether that changed anything.
    fn tick(&mut self, now: Instant) -> bool;

    /// Whether anything on it is moving, so frames come at a frame's pace.
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

    /// Up again where it was set aside rather than taken down: the picker,
    /// back from the help or the settings it went across to.
    fn back(&mut self) {}

    /// Where it goes once it is down, if not back to the session.
    fn leave(&self) -> Option<Leave> {
        None
    }

    /// Whether it has asked to be taken down with no key or gesture to say
    /// so: the picker's half-typed number, settled by the clock, and the gap
    /// `c` opens, once it is up.
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
