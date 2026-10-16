//! What floats over a session in a panel the relay draws (see
//! `crate::pty::panel`): the settings ([`super::settings::Panel`]) and the
//! help ([`super::help::Panel`]).
//!
//! The relay reads the terminal while one is up, the screen being drawn over a
//! session that has it, and hands each its keys and the mouse; it asks where
//! the screen goes, draws it into a buffer of that size, and lays the buffer
//! over the session dimmed round it. What floats keeps its own rules, every
//! one, as it has them full screen: what changes is where it is drawn.

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

/// A screen that floats over a session.
pub trait Floating {
    /// Where it floats on `screen`, or `None` where there is no room for it
    /// and the session round it: the relay then shows it full screen.
    fn area(&self, screen: Rect) -> Option<Rect>;

    /// A key, as the terminal reported it.
    fn on_key(&mut self, key: KeyEvent, now: Instant) -> Did;

    /// A mouse report inside the screen or out of it; a press outside is the
    /// relay's, which takes the screen down.
    fn on_mouse(&mut self, mouse: MouseEvent, now: Instant) -> Did;

    /// When something on it changes on its own, if anything is to.
    fn wake_at(&self) -> Option<Instant>;

    /// Moved on to `now`: whether that changed anything.
    fn tick(&mut self, now: Instant) -> bool;

    /// Whether anything on it is moving, so frames come at a frame's pace.
    fn moving(&self) -> bool;

    /// The screen drawn into `area`, a buffer of that area.
    fn draw(&mut self, area: Rect) -> Buffer;

    /// Taken down.
    fn close(&self);
}

/// Where a screen's parts go in the panel it floats in: all of it but a
/// column either side and the row at the bottom, which keep its text off the
/// session's.
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

    /// A column off either side and the last row off the bottom.
    #[test]
    fn the_inside_leaves_a_column_either_side_and_the_last_row() {
        assert_eq!(inside(Rect::new(8, 3, 64, 18)), Rect::new(9, 3, 62, 17));
        assert_eq!(inside(Rect::new(0, 0, 1, 1)), Rect::new(1, 0, 0, 0));
    }
}
