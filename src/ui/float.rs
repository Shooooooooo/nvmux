//! Screens drawn in a panel over a session (see `crate::pty::panel`): the
//! settings ([`super::settings::Panel`]), the help ([`super::help::Panel`]),
//! the picker ([`super::picker::Panel`]) and the naming prompt
//! ([`super::prompt::Panel`]).
//!
//! While one is up the relay reads the terminal, forwards keys and mouse to
//! it, draws it into a buffer of the size it asks for, and lays that over the
//! dimmed session. Each screen behaves as it does full screen.
//!
//! The hint row does not float: it stays on the terminal's last row, full
//! width, as full screen ([`Floating::draw_hints`]), with the panel in the
//! room above it ([`split`]). The panel's corners are round ([`RADIUS`]).

use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyEvent, MouseEvent};
use ratatui::layout::Rect;

/// How far a panel's corners are rounded, in columns: about two rows, so the
/// arc is round on screen. It cuts four cells off each end of the first and
/// last rows and the margin column ([`inside`]) of the rows next to them,
/// which every screen leaves blank.
pub const RADIUS: f32 = 4.0;

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
    /// Where it floats on `screen`, in the room above the hint row
    /// ([`split`]), or `None` if there is no room around it; the relay then
    /// shows it full screen.
    fn area(&self, screen: Rect) -> Option<Rect>;

    fn on_key(&mut self, key: KeyEvent, now: Instant) -> Did;

    /// Gets every event but a left press on the session (outside the panel
    /// and hint row), which the relay handles by closing the screen.
    fn on_mouse(&mut self, mouse: MouseEvent, now: Instant) -> Did;

    /// When it next changes on its own, if ever.
    fn wake_at(&self) -> Option<Instant>;

    /// Advance to `now`; returns whether anything changed.
    fn tick(&mut self, now: Instant) -> bool;

    /// Whether it is animating, so frames run at the frame rate.
    fn moving(&self) -> bool;

    /// Drawn into a buffer of `area`, all but its hint row (see [`inside`]).
    fn draw(&mut self, area: Rect) -> Buffer;

    /// The hint row drawn into `row` (the screen's last row, see [`split`])
    /// as full screen on a terminal as wide, blank either side so none of the
    /// session's last row shows.
    fn draw_hints(&self, row: Rect) -> Buffer;

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

/// The room for a floating panel, and the hint row under it: the last row,
/// full width, as full screen ([`super::draw::split_hint_row`]). Both empty
/// if `screen` has no rows.
pub fn split(screen: Rect) -> (Rect, Rect) {
    if screen.height == 0 {
        return (screen, screen);
    }
    super::draw::split_hint_row(screen)
}

/// How far (`col`, `row`) is from `panel`, its corners rounded, in columns
/// (see [`crate::falloff::distance`]); 0 on it.
pub fn distance(panel: Rect, col: u16, row: u16) -> f32 {
    crate::falloff::distance(
        col,
        row,
        panel.x,
        panel.y,
        panel.width,
        panel.height,
        RADIUS,
    )
}

/// The panel minus a column each side, which keep its text off the
/// session's. The screen is drawn as on a terminal this size, with its last
/// row, where the hint row would go, left blank as a margin ([`lay`]).
pub(super) fn inside(panel: Rect) -> Rect {
    Rect::new(
        panel.x + 1,
        panel.y,
        panel.width.saturating_sub(2),
        panel.height,
    )
}

/// Copies `drawn`, the screen drawn on a terminal the size of `inside`, into
/// `buf` at `inside`: all but its last row, the hint row's.
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

    #[test]
    fn the_inside_leaves_a_column_either_side() {
        assert_eq!(inside(Rect::new(8, 3, 64, 18)), Rect::new(9, 3, 62, 18));
        assert_eq!(inside(Rect::new(0, 0, 1, 1)), Rect::new(1, 0, 0, 1));
    }

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
