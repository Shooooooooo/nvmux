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
//! # A switch's card
//!
//! A switch by key carries the card it lands with in the same way (see
//! [`crate::card`]): the session it leaves has its own client draw the card as
//! the switch begins, and that session's fade out leaves it standing, at its
//! full colour; the screen it leaves for the next session's client is cleared
//! with the card still on it, as the picker leaves its name; and the next
//! session's client draws the card again where it stands, before its screen
//! dissolves in, which leaves it standing too ([`crate::shadow::Kept`]). So
//! the card is on the screen as it is from the key to the session it names,
//! and takes no part in either fade. It is no overlay: the session it goes to
//! draws it itself.
//!
//! # The screen the shell comes back to
//!
//! Between the picker and the session, the terminal is on its primary screen —
//! the one the shell sees again on a detach — and the name is drawn there, as
//! a switch's card is.
//! Nothing nvmux does afterwards clears that screen on its own, so whoever
//! takes the terminal next takes the name down first: the session's first
//! frame, in the same synchronized write as its switch to the alternate
//! screen; the attaching screen, before its spinner; and every path that ends
//! an attach before it begins.

use unicode_width::UnicodeWidthStr;

use crate::palette::Rgb;
use crate::shadow::Over;

/// What is left standing for the session an attach goes to: the picker's
/// name for it, or the card a switch by key lands with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandOff {
    left: Left,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Left {
    /// The name, and where it stands: what the picker hands to the session,
    /// which opens out of it.
    Name {
        /// The name as it was drawn — cut where the picker's row cut it, and
        /// with anything that is not text taken out (see
        /// [`crate::announce::label`]), since this reaches a raw terminal.
        text: String,
        /// The top-left cell, 0-based, in the terminal's own grid.
        row: u16,
        col: u16,
        /// What the name is drawn in: the theme's ink, which the picker drew
        /// it in, or `None` for the terminal's own foreground (see
        /// [`crate::theme`]).
        ink: Option<Rgb>,
    },
    /// The card a switch by key lands with, as the session it left drew it
    /// and its fade out left it standing (see [`crate::shadow::Kept`]): the
    /// bytes that draw it, and the cells they draw on, each span a row, its
    /// first column and its last. The session it lands in draws the card
    /// again where it stands, and stands it through its fade in.
    Card {
        drawn: Vec<u8>,
        spans: Vec<(u16, u16, u16)>,
    },
}

impl HandOff {
    /// The name `name` at (`row`, `col`), cut to `width` columns, in the
    /// theme's ink if there is one. `None` when nothing of it is left to show.
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

    /// The card a switch lands with, as `drawn` draws it on the cells of
    /// `spans` (see [`crate::shadow::Shadow::kept_bytes`]). `None` when there
    /// is nothing to draw.
    pub fn card(drawn: Vec<u8>, spans: &[(u16, u16, u16)]) -> Option<Self> {
        (!drawn.is_empty() && !spans.is_empty()).then(|| Self {
            left: Left::Card {
                drawn,
                spans: spans.to_vec(),
            },
        })
    }

    /// Whether config allows the hand-off and the terminal can show it: the
    /// fade, the session's own fade, and `[effects.attach]`.
    pub fn enabled() -> bool {
        crate::config::get().effects.attach_enabled()
            && crate::fade::enabled()
            && crate::fade::session()
    }

    /// The name, for the picker's; nothing for a card.
    pub fn text(&self) -> &str {
        match &self.left {
            Left::Name { text, .. } => text,
            Left::Card { .. } => "",
        }
    }

    /// The cell it starts on, 0-based: row, then column.
    pub fn at(&self) -> (u16, u16) {
        match &self.left {
            Left::Name { row, col, .. } => (*row, *col),
            Left::Card { spans, .. } => spans.first().map_or((0, 0), |&(row, from, _)| (row, from)),
        }
    }

    /// Columns the name takes, for the picker's; the first of a card's rows.
    pub fn width(&self) -> u16 {
        match &self.left {
            Left::Name { text, .. } => u16::try_from(text.width()).unwrap_or(u16::MAX),
            Left::Card { spans, .. } => spans
                .first()
                .map_or(0, |&(_, from, to)| to.saturating_sub(from) + 1),
        }
    }

    /// The name as an overlay for the session's fade-in, or `None` when it
    /// would not fit a screen `rows` by `cols` — a terminal resized during the
    /// attach — since a name half off the edge is better not drawn at all.
    /// None for a card, which the session's own client draws again itself.
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

    /// The bytes that draw it as it was left — the name in the terminal's own
    /// colours, or in the theme's ink; the card in its own — leaving the
    /// cursor and the pen as they were.
    pub fn show(&self) -> Vec<u8> {
        match &self.left {
            Left::Name {
                text,
                row,
                col,
                ink,
            } => at_cell(*row, *col, text, *ink),
            Left::Card { drawn, .. } => drawn.clone(),
        }
    }

    /// The bytes that take it down again: blanks over its cells, in the
    /// terminal's own background, leaving the cursor and the pen as they were.
    pub fn erase(&self) -> Vec<u8> {
        match &self.left {
            Left::Name { row, col, .. } => {
                at_cell(*row, *col, &" ".repeat(usize::from(self.width())), None)
            }
            Left::Card { spans, .. } => {
                let mut out = format!("\x1b7{}", crate::announce::pen(None));
                for &(row, from, to) in spans {
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

    /// Take it down now: [`HandOff::erase`], written. For the paths that end
    /// an attach before the session has a frame to take it down with.
    pub fn take_down(&self) {
        let _ = crate::term::write_stdout(&self.erase());
    }
}

/// `text` written at (`row`, `col`): `DECSC`, a reset so the cells are the
/// terminal's own colours — carrying `ink` when there is one (see
/// [`crate::announce::pen`]) — an absolute `CUP` — no newline, which with
/// `OPOST` off would move the cursor rather than wrap — and `DECRC`.
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

    /// Under a theme the name is left in the ink the picker drew it in; it is
    /// taken down the same either way, blanks showing no colour.
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

    /// A switch's card is drawn as the session it left drew it, and taken
    /// down cell for cell; it is no overlay, which the session it goes to
    /// draws again for itself.
    #[test]
    fn a_card_is_drawn_as_it_was_and_taken_down_cell_for_cell() {
        let drawn = b"\x1b7\x1b[2;90H\x1b[0;1;38;2;1;2;3mblog\x1b8".to_vec();
        let h = HandOff::card(drawn.clone(), &[(1, 85, 96), (2, 90, 96)]).expect("a card");
        assert_eq!(h.show(), drawn);
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
            HandOff::card(Vec::new(), &[(1, 1, 2)]).is_none(),
            "nothing drawn"
        );
        assert!(HandOff::card(drawn, &[]).is_none(), "nowhere");
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
