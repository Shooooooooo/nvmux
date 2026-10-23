//! The hand-off: the attached session's name, kept on screen from the picker
//! into the session.
//!
//! 1. The picker's fade-out dissolves everything but the name, closing onto
//!    its row ([`crate::fade::fade_out_keeping`], [`crate::fade::Iris`]).
//! 2. The screen is cleared with the name still on it in one synchronized
//!    write (`close_for_attach` in [`crate::ui`]), and it stays through the
//!    probe and the held first paint.
//! 3. The session opens out from the name's row, with the name as an overlay
//!    that dissolves out ([`crate::shadow::Shadow::under`]).
//!
//! It adds no frames, only rides the fades' own, so it needs the fade and
//! `[effects.fade] session` as well as `[effects.attach]`; with any off, or
//! `NO_COLOR`, an attach is plain.
//!
//! A switch by key carries its [`crate::card`] the same way, except that the
//! new client redraws it in place before fading in ([`crate::shadow::Kept`])
//! rather than as an overlay, timed from when it first went up
//! ([`crate::card::Card::up_ms`]).
//!
//! Either is drawn on the primary screen, which the shell sees after a detach,
//! so whoever takes the terminal next erases it: the session's first frame (in
//! the write that switches to the alternate screen), the attaching screen
//! before its spinner, and every path that ends an attach early.

use std::time::Instant;

use unicode_width::UnicodeWidthStr;

use crate::palette::Rgb;
use crate::shadow::Over;

/// What is left on screen for the session being attached: the picker's name
/// or a switch's card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandOff {
    left: Left,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Left {
    Name {
        /// Truncated as the picker's row was, and sanitized
        /// ([`crate::announce::label`]) since it reaches a raw terminal.
        text: String,
        /// Top-left cell, 0-based.
        row: u16,
        col: u16,
        /// The theme's ink, or `None` for the terminal's foreground.
        ink: Option<Rgb>,
    },
    /// The bytes that draw the card, and its cells as `(row, first, last)`
    /// column spans. `since` is when it went up. Boxed slices keep this
    /// variant as small as `Name`.
    Card {
        drawn: Box<[u8]>,
        spans: Box<[(u16, u16, u16)]>,
        since: Instant,
    },
}

impl HandOff {
    /// `name` at (`row`, `col`), cut to `width` columns. `None` when nothing
    /// is left to show.
    pub fn new(name: &str, row: u16, col: u16, width: u16) -> Option<Self> {
        let text = crate::ui::draw::truncate(&crate::announce::label(name), usize::from(width));
        (!text.is_empty()).then(|| Self {
            left: Left::Name {
                text,
                row,
                col,
                ink: crate::theme::ink(),
            },
        })
    }

    /// A switch's card (see [`crate::shadow::Shadow::kept_bytes`]). `None`
    /// when there is nothing to draw.
    pub fn card(drawn: Vec<u8>, spans: &[(u16, u16, u16)], since: Instant) -> Option<Self> {
        (!drawn.is_empty() && !spans.is_empty()).then(|| Self {
            left: Left::Card {
                drawn: drawn.into(),
                spans: spans.into(),
                since,
            },
        })
    }

    /// When a card went up, for [`crate::card::Card::up_ms`].
    pub fn up_since(&self) -> Option<Instant> {
        match &self.left {
            Left::Card { since, .. } => Some(*since),
            Left::Name { .. } => None,
        }
    }

    /// Needs `[effects.attach]`, the fade, and the session fade.
    pub fn enabled() -> bool {
        crate::config::get().effects.attach_enabled()
            && crate::fade::enabled()
            && crate::fade::session()
    }

    pub fn text(&self) -> &str {
        match &self.left {
            Left::Name { text, .. } => text,
            Left::Card { .. } => "",
        }
    }

    /// First cell, 0-based `(row, col)`.
    pub fn at(&self) -> (u16, u16) {
        match &self.left {
            Left::Name { row, col, .. } => (*row, *col),
            Left::Card { spans, .. } => spans.first().map_or((0, 0), |&(row, from, _)| (row, from)),
        }
    }

    /// The name's width in columns, or that of a card's first row.
    pub fn width(&self) -> u16 {
        match &self.left {
            Left::Name { text, .. } => u16::try_from(text.width()).unwrap_or(u16::MAX),
            Left::Card { spans, .. } => spans
                .first()
                .map_or(0, |&(_, from, to)| to.saturating_sub(from) + 1),
        }
    }

    /// The name as an overlay for the session's fade-in. `None` if it no
    /// longer fits `rows` x `cols` (terminal resized mid-attach), or for a
    /// card, which the client redraws itself.
    pub fn over(&self, rows: u16, cols: u16) -> Option<Over> {
        let Left::Name { text, row, col, .. } = &self.left else {
            return None;
        };
        let width = self.width();
        let fits = *row < rows && u32::from(*col) + u32::from(width) <= u32::from(cols);
        fits.then(|| Over {
            top: *row,
            left: *col,
            width,
            rows: vec![text.clone()],
        })
    }

    /// Bytes that draw it, preserving cursor and pen.
    pub fn show(&self) -> Vec<u8> {
        match &self.left {
            Left::Name {
                text,
                row,
                col,
                ink,
            } => at_cell(*row, *col, text, *ink),
            Left::Card { drawn, .. } => drawn.to_vec(),
        }
    }

    /// Bytes that blank its cells in the default background, preserving cursor
    /// and pen.
    pub fn erase(&self) -> Vec<u8> {
        match &self.left {
            Left::Name { row, col, .. } => {
                at_cell(*row, *col, &" ".repeat(usize::from(self.width())), None)
            }
            Left::Card { spans, .. } => {
                let mut out = format!("\x1b7{}", crate::announce::pen(None));
                for &(row, from, to) in spans.iter() {
                    out.push_str(&format!(
                        "\x1b[{};{}H{}",
                        u32::from(row) + 1,
                        u32::from(from) + 1,
                        " ".repeat(usize::from(to.saturating_sub(from)) + 1)
                    ));
                }
                out.push_str("\x1b8");
                out.into_bytes()
            }
        }
    }

    /// Write [`HandOff::erase`] now, for paths that end an attach before the
    /// session draws a frame.
    pub fn take_down(&self) {
        let _ = crate::term::write_stdout(&self.erase());
    }
}

/// `DECSC`, pen reset (with `ink` if any), absolute `CUP`, `text`, `DECRC`. No
/// newline: with `OPOST` off it would move the cursor rather than wrap.
fn at_cell(row: u16, col: u16, text: &str, ink: Option<Rgb>) -> Vec<u8> {
    format!(
        "\x1b7{}\x1b[{};{}H{text}\x1b8",
        crate::announce::pen(ink),
        u32::from(row) + 1,
        u32::from(col) + 1
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dotfiles() -> HandOff {
        HandOff::new("dotfiles", 4, 27, 20).expect("a name")
    }

    #[test]
    fn the_name_is_drawn_where_it_stood() {
        let shown = String::from_utf8(dotfiles().show()).unwrap();
        assert_eq!(shown, "\x1b7\x1b[0m\x1b[5;28Hdotfiles\x1b8");
    }

    #[test]
    fn taking_it_down_blanks_exactly_its_cells() {
        let erased = String::from_utf8(dotfiles().erase()).unwrap();
        assert_eq!(erased, "\x1b7\x1b[0m\x1b[5;28H        \x1b8");
    }

    /// Erasing ignores the ink.
    #[test]
    fn a_themed_name_is_left_in_its_ink() {
        let h = HandOff {
            left: Left::Name {
                text: "dotfiles".into(),
                row: 4,
                col: 27,
                ink: Some(Rgb(0x7a, 0xa2, 0xf7)),
            },
        };
        let shown = String::from_utf8(h.show()).unwrap();
        assert_eq!(
            shown,
            "\x1b7\x1b[0;38;2;122;162;247m\x1b[5;28Hdotfiles\x1b8"
        );
        assert_eq!(h.erase(), dotfiles().erase());
    }

    #[test]
    fn it_is_cut_to_the_room_the_row_had() {
        let h = HandOff::new("infra-staging-eu-west-1", 0, 0, 5).unwrap();
        assert_eq!(h.text(), "infra");
        let wide = HandOff::new("日本語", 0, 0, 5).unwrap();
        assert_eq!(wide.text(), "日本");
        assert_eq!(wide.width(), 4);
        assert!(HandOff::new("x", 0, 0, 0).is_none(), "no room, no name");
    }

    #[test]
    fn control_characters_never_reach_the_terminal() {
        let h = HandOff::new("evil\x1b[2Jname", 0, 0, 40).unwrap();
        assert!(!h.text().contains('\x1b'), "{:?}", h.text());
        assert!(
            HandOff::new("\x07\x1b", 0, 0, 40).is_none(),
            "nothing left of it"
        );
    }

    #[test]
    fn a_card_is_drawn_as_it_was_and_taken_down_cell_for_cell() {
        let drawn = b"\x1b7\x1b[2;90H\x1b[0;1;38;2;1;2;3mblog\x1b8".to_vec();
        let up = Instant::now();
        let h = HandOff::card(drawn.clone(), &[(1, 85, 96), (2, 90, 96)], up).expect("a card");
        assert_eq!(h.show(), drawn);
        assert_eq!(
            h.up_since(),
            Some(up),
            "up since it went up on the screen left"
        );
        assert_eq!(
            String::from_utf8(h.erase()).unwrap(),
            format!(
                "\x1b7\x1b[0m\x1b[2;86H{}\x1b[3;91H{}\x1b8",
                " ".repeat(12),
                " ".repeat(7)
            )
        );
        assert_eq!(h.over(24, 100), None);
        assert!(
            HandOff::card(Vec::new(), &[(1, 1, 2)], up).is_none(),
            "nothing drawn"
        );
        assert!(HandOff::card(drawn, &[], up).is_none(), "nowhere");
    }

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
