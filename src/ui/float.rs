//! Screens drawn in a panel over a session (see `crate::pty::panel`): the
//! settings ([`super::settings::Panel`]), the help ([`super::help::Panel`])
//! and the picker ([`super::picker::Panel`]).
//!
//! While one is up the relay reads the terminal, forwards keys and mouse to
//! it, draws it into a buffer of the size it asks for, and lays that over the
//! dimmed session. Each screen behaves as it does full screen.

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
    /// Where it floats on `screen`, or `None` if there is no room around it;
    /// the relay then shows it full screen.
    fn area(&self, screen: Rect) -> Option<Rect>;

    fn on_key(&mut self, key: KeyEvent, now: Instant) -> Did;

    /// Gets events outside the screen too, except a left press there, which
    /// the relay handles by closing the screen.
    fn on_mouse(&mut self, mouse: MouseEvent, now: Instant) -> Did;

    /// When it next changes on its own, if ever.
    fn wake_at(&self) -> Option<Instant>;

    /// Advance to `now`; returns whether anything changed.
    fn tick(&mut self, now: Instant) -> bool;

    /// Whether it is animating, so frames run at the frame rate.
    fn moving(&self) -> bool;

    fn draw(&mut self, area: Rect) -> Buffer;

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

/// The panel minus a column each side and the bottom row, which keep its text
/// off the session's.
pub(super) fn inside(panel: Rect) -> Rect {
    Rect::new(
        panel.x + 1,
        panel.y,
        panel.width.saturating_sub(2),
        panel.height.saturating_sub(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_inside_leaves_a_column_either_side_and_the_last_row() {
        assert_eq!(inside(Rect::new(8, 3, 64, 18)), Rect::new(9, 3, 62, 17));
        assert_eq!(inside(Rect::new(0, 0, 1, 1)), Rect::new(1, 0, 0, 0));
    }
}
