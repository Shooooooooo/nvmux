//! The hand-off: the name of the session being attached to, kept on the screen
//! from the picker into the session.
//!
//! An attach from the picker used to be three separate pictures — the picker
//! dissolving, an empty screen for as long as the client took to start, and
//! the session dissolving in — with nothing carried from one to the next. The
//! hand-off carries the one thing all three are about: the session's name.
//!
//! 1. The picker's fade-out dissolves everything but the name, closing in from
//!    the top and the bottom onto its row, while the name comes off the
//!    selection's bar as plain text where it stood (see
//!    [`crate::fade::fade_out_keeping`] and [`crate::fade::Iris`]).
//! 2. The screen the picker leaves for the client spawn is cleared with the
//!    name still on it, in the one synchronized write, so the clear and the
//!    name are never presented apart (see [`crate::ui`]'s `close_for_attach`).
//!    It stays there through the probe and the first paint being held.
//! 3. The session opens back out of the line the name stands on, that line
//!    first and the rows furthest from it last, with the name painted over its
//!    frames as an overlay, dissolving out as the session dissolves in — the
//!    cross-dissolve the attach notice uses, from
//!    [`crate::shadow::Shadow::under`].
//!
//! No frame is added to an attach: the name rides frames that were already
//! being drawn. So it needs the fade, and the session's own fade with it
//! (`[effects.fade] session`); with either off, or `NO_COLOR`, there are no
//! frames to ride and an attach is what it was.
//!
//! # The screen the shell comes back to
//!
//! Between the picker and the session, the terminal is on its primary screen —
//! the one the shell sees again on a detach — and the name is drawn there.
//! Nothing nvmux does afterwards clears that screen on its own, so whoever
//! takes the terminal next takes the name down first: the session's first
//! frame, in the same synchronized write as its switch to the alternate
//! screen; the attaching screen, before its spinner; and every path that ends
//! an attach before it begins.

use unicode_width::UnicodeWidthStr;

use crate::shadow::Over;

/// The name, and where it stands: what the picker hands to the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandOff {
    /// The name as it was drawn — cut where the picker's row cut it, and with
    /// anything that is not text taken out (see [`crate::announce::label`]),
    /// since this reaches a raw terminal.
    text: String,
    /// The top-left cell, 0-based, in the terminal's own grid.
    row: u16,
    col: u16,
}

impl HandOff {
    /// The name `name` at (`row`, `col`), cut to `width` columns. `None` when
    /// nothing of it is left to show.
    pub fn new(name: &str, row: u16, col: u16, width: u16) -> Option<Self> {
        let text = crate::ui::draw::truncate(&crate::announce::label(name), usize::from(width));
        (!text.is_empty()).then_some(Self { text, row, col })
    }

    /// Whether config allows the hand-off and the terminal can show it: the
    /// fade, the session's own fade, and `[effects.attach]`.
    pub fn enabled() -> bool {
        crate::config::get().effects.attach_enabled()
            && crate::fade::enabled()
            && crate::fade::session()
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// The cell the name starts on, 0-based: row, then column.
    pub fn at(&self) -> (u16, u16) {
        (self.row, self.col)
    }

    /// Columns the name takes.
    pub fn width(&self) -> u16 {
        u16::try_from(self.text.width()).unwrap_or(u16::MAX)
    }

    /// The name as an overlay for the session's fade-in, or `None` when it
    /// would not fit a screen `rows` by `cols` — a terminal resized during the
    /// attach — since a name half off the edge is better not drawn at all.
    pub fn over(&self, rows: u16, cols: u16) -> Option<Over> {
        let fits =
            self.row < rows && u32::from(self.col) + u32::from(self.width()) <= u32::from(cols);
        fits.then(|| Over {
            top: self.row,
            left: self.col,
            width: self.width(),
            rows: vec![self.text.clone()],
        })
    }

    /// The bytes that draw the name in the terminal's own colours, leaving the
    /// cursor and the pen as they were.
    pub fn show(&self) -> Vec<u8> {
        self.at_name(&self.text)
    }

    /// The bytes that take the name down again: blanks over its cells, in the
    /// terminal's own background, leaving the cursor and the pen as they were.
    pub fn erase(&self) -> Vec<u8> {
        self.at_name(&" ".repeat(usize::from(self.width())))
    }

    /// Take the name down now: [`HandOff::erase`], written. For the paths that
    /// end an attach before the session has a frame to take it down with.
    pub fn take_down(&self) {
        let _ = crate::term::write_stdout(&self.erase());
    }

    /// `text` written over the name's cells: `DECSC`, a reset so the cells
    /// are the terminal's own colours, an absolute `CUP` — no newline, which
    /// with `OPOST` off would move the cursor rather than wrap — and `DECRC`.
    fn at_name(&self, text: &str) -> Vec<u8> {
        format!(
            "\x1b7\x1b[0m\x1b[{};{}H{text}\x1b8",
            u32::from(self.row) + 1,
            u32::from(self.col) + 1
        )
        .into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dotfiles() -> HandOff {
        HandOff::new("dotfiles", 4, 27, 20).expect("a name")
    }

    /// The name goes where the picker had it, 1-based on the wire, and the
    /// cursor and pen are put back around it.
    #[test]
    fn the_name_is_drawn_where_it_stood() {
        let shown = String::from_utf8(dotfiles().show()).unwrap();
        assert_eq!(shown, "\x1b7\x1b[0m\x1b[5;28Hdotfiles\x1b8");
    }

    /// Taking it down blanks exactly its cells.
    #[test]
    fn taking_it_down_blanks_exactly_its_cells() {
        let erased = String::from_utf8(dotfiles().erase()).unwrap();
        assert_eq!(erased, "\x1b7\x1b[0m\x1b[5;28H        \x1b8");
    }

    /// Cut where the row was cut, measured in columns.
    #[test]
    fn it_is_cut_to_the_room_the_row_had() {
        let h = HandOff::new("infra-staging-eu-west-1", 0, 0, 5).unwrap();
        assert_eq!(h.text(), "infra");
        let wide = HandOff::new("日本語", 0, 0, 5).unwrap();
        assert_eq!(wide.text(), "日本");
        assert_eq!(wide.width(), 4);
        assert!(HandOff::new("x", 0, 0, 0).is_none(), "no room, no name");
    }

    /// A name smuggling a control sequence in reaches the terminal as text
    /// without it.
    #[test]
    fn control_characters_never_reach_the_terminal() {
        let h = HandOff::new("evil\x1b[2Jname", 0, 0, 40).unwrap();
        assert!(!h.text().contains('\x1b'), "{:?}", h.text());
        assert!(
            HandOff::new("\x07\x1b", 0, 0, 40).is_none(),
            "nothing left of it"
        );
    }

    /// As an overlay, the name is one row of exactly its width; on a screen
    /// that has shrunk under it, it is not drawn at all.
    #[test]
    fn as_an_overlay_it_fits_or_it_is_not_drawn() {
        let over = dotfiles().over(24, 80).expect("fits");
        assert_eq!((over.top, over.left, over.width), (4, 27, 8));
        assert_eq!(over.rows, vec!["dotfiles".to_string()]);
        assert!(dotfiles().over(4, 80).is_none(), "below the last row");
        assert!(dotfiles().over(24, 34).is_none(), "off the right edge");
        assert!(dotfiles().over(24, 35).is_some(), "exactly to the edge");
    }
}
