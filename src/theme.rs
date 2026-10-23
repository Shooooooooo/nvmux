//! The single colour (the *ink*) that `[style] color` draws nvmux's own
//! screens in; they otherwise use the terminal's default foreground.
//!
//! [`paint`] is a pass over a finished frame, run before the picker's effects
//! and the fade, so the screens' drawing (and its tests) stay colourless. It
//! inks full-strength cells with no colour of their own; dim cells keep the
//! terminal's foreground, and no background is ever set. The fade and effects
//! start from the colour a cell was drawn in ([`crate::fade::drawn_in`]), so a
//! themed screen dissolves without a jump. The switch notice and attach name
//! write the ink into their pen ([`crate::announce::pen`]).
//!
//! Neovim's screen and the first-run screen ([`crate::ui::setup`], shown before
//! any config exists) are never inked. `NO_COLOR` turns the theme off.

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};

use crate::config::ThemeColor;
use crate::palette::Rgb;

/// `None` means the terminal's own colour: no `[style] color`, or `NO_COLOR`.
pub fn ink() -> Option<Rgb> {
    configured(
        crate::config::get().style.color,
        std::env::var_os("NO_COLOR").is_some(),
    )
}

/// [`ink`] without the process globals. `NO_COLOR` wins over the config.
fn configured(color: ThemeColor, no_color: bool) -> Option<Rgb> {
    match color {
        ThemeColor::Mono(ink) if !no_color => Some(ink),
        ThemeColor::Mono(_) | ThemeColor::Terminal => None,
    }
}

/// Ink every full-strength cell of `buf` that has no colour of its own, if
/// there is an ink.
pub fn paint(buf: &mut Buffer) {
    if let Some(ink) = ink() {
        paint_in(buf, ink);
    }
}

/// [`paint`] with a given ink. Blank cells are inked too, so reversed or
/// underlined ones show it as their bar or line. Set foregrounds, backgrounds
/// and modifiers are left alone.
pub(crate) fn paint_in(buf: &mut Buffer, ink: Rgb) {
    let colour = Color::Rgb(ink.0, ink.1, ink.2);
    for cell in &mut buf.content {
        if cell.fg == Color::Reset && !cell.modifier.contains(Modifier::DIM) {
            cell.set_fg(colour);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fade::{self, drawn_in};
    use crate::test_support::palette as terminal;
    use ratatui::layout::Rect;
    use ratatui::style::Style;

    const INK: Rgb = Rgb(0x7a, 0xa2, 0xf7);

    fn colour(c: Rgb) -> Color {
        Color::Rgb(c.0, c.1, c.2)
    }

    #[test]
    fn the_ink_is_the_configured_colour_unless_no_color_says_none() {
        assert_eq!(configured(ThemeColor::Mono(INK), false), Some(INK));
        assert_eq!(configured(ThemeColor::Mono(INK), true), None);
        assert_eq!(configured(ThemeColor::Terminal, false), None);
        assert_eq!(configured(ThemeColor::Terminal, true), None);
    }

    /// No config is installed under test, so other tests see no ink.
    #[test]
    fn under_test_there_is_no_ink() {
        assert_eq!(ink(), None);
    }

    #[test]
    fn only_full_strength_text_with_no_colour_takes_the_ink() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 12, 2));
        let dim = Style::default().add_modifier(Modifier::DIM);
        let bar = Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD);
        buf.set_string(0, 0, "1", dim);
        buf.set_string(2, 0, "notes", bar);
        buf.set_string(0, 1, "set", Style::default().fg(Color::Rgb(1, 2, 3)));
        let before = buf.clone();

        paint_in(&mut buf, INK);

        for (cell, was) in buf.content.iter().zip(&before.content) {
            assert_eq!(cell.bg, Color::Reset, "set a background: {cell:?}");
            assert_eq!(cell.modifier, was.modifier, "touched a modifier");
            assert_eq!(cell.symbol(), was.symbol());
            let want = if was.fg == Color::Reset && !was.modifier.contains(Modifier::DIM) {
                colour(INK)
            } else {
                was.fg
            };
            assert_eq!(cell.fg, want, "{cell:?}");
        }
        assert_eq!(buf[(0, 0)].fg, Color::Reset, "the dim number kept its own");
        assert_eq!(buf[(2, 0)].fg, colour(INK), "a bar of ink");
    }

    /// Full-strength cells fade from the ink and dim ones from the terminal's
    /// foreground, so neither jumps on the fade's first frame.
    #[test]
    fn a_themed_screen_dissolves_out_of_the_colours_it_is_drawn_in() {
        let p = terminal();
        let draw = |t: f32| {
            let mut buf = Buffer::empty(Rect::new(0, 0, 12, 1));
            buf.set_string(0, 0, "1", Style::default().add_modifier(Modifier::DIM));
            buf.set_string(2, 0, "dotfiles", Style::default());
            paint_in(&mut buf, INK);
            fade::apply(&mut buf, &p, t);
            buf
        };

        let drawn = draw(0.0);
        assert_eq!(drawn_in(&drawn[(2, 0)], &p), INK, "the name, as drawn");
        assert_eq!(drawn_in(&drawn[(0, 0)], &p), p.fg, "the number, as drawn");

        let first = draw(0.01);
        assert_eq!(first[(2, 0)].fg, colour(INK.lerp(p.bg, 0.01)));
        assert_eq!(first[(0, 0)].fg, colour(p.fg.lerp(p.bg, 0.01)));
        assert!(first[(0, 0)].modifier.contains(Modifier::DIM), "still dim");
        assert_ne!(INK, p.fg, "the fixture's ink must differ to say anything");
    }
}
