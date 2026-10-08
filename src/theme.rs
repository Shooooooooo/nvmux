//! `[theme]`: one colour, the background of nvmux's own screens and the one
//! the picker's effects paint in.
//!
//! nvmux's screens draw no colour of their own (see [`crate::ui`]), and that
//! stays true with a theme: the background is laid as a post-pass over a
//! finished frame, the way [`crate::ui::effects`] and [`crate::fade`] lay
//! theirs, so `draw` and its siblings — and their tests — never see it. With
//! no `[theme] background`, or under `NO_COLOR`, the pass sets nothing, and a
//! frame is byte for byte what it was before there was a theme.
//!
//! # Order
//!
//! The background goes on last of the things a plain frame is drawn with:
//! after the effects, which lay the sonar's rings only on cells nothing has
//! painted yet and give the rows they colour a background of their own, which
//! this leaves alone. In a fade it goes on before the dissolve, which carries
//! it with everything else into the terminal's own background (see
//! [`crate::fade::apply`]).
//!
//! The effects' colours are the same one, in shades: [`crate::ui::effects::Ink`]
//! paints each of their cells in the theme's colour, as bright as the cell
//! would have been without it.

use ratatui::buffer::Buffer;
use ratatui::style::Color;
use ratatui::Frame;

use crate::palette::Rgb;

/// The background nvmux's own screens are drawn on, or `None` for the
/// terminal's own: none set, or `NO_COLOR`.
pub fn background() -> Option<Rgb> {
    if std::env::var_os("NO_COLOR").is_some() {
        return None;
    }
    crate::config::get().theme.background
}

/// Lay `bg` behind every cell of `buf` that has no background of its own. A
/// cell something has already coloured — an effect's bar, a fade's — keeps
/// its colour, and with `None` nothing is touched.
pub fn fill(buf: &mut Buffer, bg: Option<Rgb>) {
    let Some(Rgb(r, g, b)) = bg else {
        return;
    };
    for cell in &mut buf.content {
        if cell.bg == Color::Reset {
            cell.set_bg(Color::Rgb(r, g, b));
        }
    }
}

/// [`fill`] the frame a screen has just drawn with the configured background.
pub fn paint(frame: &mut Frame) {
    fill(frame.buffer_mut(), background());
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use ratatui::style::{Modifier, Style};

    const BG: Rgb = Rgb(0x1e, 0x1e, 0x2e);

    #[test]
    fn the_background_goes_behind_every_cell_nothing_coloured() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 2));
        buf[(0, 0)].set_symbol("a");
        buf[(1, 0)].set_style(Style::default().add_modifier(Modifier::REVERSED));
        buf[(2, 0)].set_bg(Color::Rgb(1, 2, 3));
        buf[(3, 0)].set_fg(Color::Rgb(4, 5, 6));
        fill(&mut buf, Some(BG));
        let themed = Color::Rgb(BG.0, BG.1, BG.2);
        assert_eq!(buf[(0, 0)].bg, themed, "behind a glyph");
        assert_eq!(buf[(1, 0)].bg, themed, "behind the bar, as its text");
        assert_eq!(buf[(2, 0)].bg, Color::Rgb(1, 2, 3), "already coloured");
        assert_eq!(buf[(3, 0)].bg, themed);
        assert_eq!(
            buf[(3, 0)].fg,
            Color::Rgb(4, 5, 6),
            "the ink is not touched"
        );
        for x in 0..4 {
            assert_eq!(buf[(x, 1)].bg, themed, "a blank row");
        }
    }

    #[test]
    fn no_background_sets_nothing() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 3, 1));
        buf[(0, 0)].set_symbol("a");
        let before = buf.clone();
        fill(&mut buf, None);
        assert_eq!(buf, before);
    }

    /// No test installs a config, so the compiled default is what every
    /// screen test draws with: no theme.
    #[test]
    fn there_is_no_background_by_default() {
        assert_eq!(crate::config::ThemeSettings::default().background, None);
        assert_eq!(background(), None);
    }
}
