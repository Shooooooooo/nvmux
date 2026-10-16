//! The colour nvmux draws its own screens in.
//!
//! nvmux's screens set no colour of their own (see [`crate::ui`]): their text
//! is the terminal's foreground, shaded with dim, bold and reverse, on the
//! terminal's background. `[style] color` names one colour to draw them in
//! instead (see [`crate::config::StyleSettings`]), and this module is where
//! that colour — the *ink* — goes on.
//!
//! # What is drawn in it, and what is not
//!
//! The theme is one colour, and it takes the screens' modifiers as it finds
//! them. [`paint`] gives the ink to every cell of a finished frame that is
//! drawn at full strength and has no colour of its own: so a name is the ink,
//! the selection's bar is the ink as a background with the name standing out
//! of it in the terminal's background colour, and an underline or a line
//! through a name is the ink because the text is. What a screen draws *dim* —
//! the numbers beside the names, the notes, the hint row, the prompt's labels
//! — it leaves alone: the terminal's own foreground, dimmed, exactly as with no
//! theme. Nothing sets a background: the terminal's own stays under
//! everything, transparent or not.
//!
//! A pass over a finished frame for the reason the fade is one
//! ([`crate::fade::apply`]): the screens' own drawing stays colourless, and its
//! tests go on saying so. It is the first thing done to a frame once the screen
//! has drawn it, before the picker's effects and the fade paint over it.
//!
//! # What fades from it
//!
//! The fade and the picker's effects paint colours of their own, and each one
//! takes a cell from the colour it is drawn in ([`crate::fade::drawn_in`]):
//! the ink this pass gave it, or for a dim cell the terminal's foreground. So
//! a themed screen dissolves out of the colours it is drawn in, the dim ones
//! with the rest, rather than jumping on the first frame. What an effect draws
//! of its own — the bar a row the cursor left lets go of — is the ink. The one
//! colour that is not the ink is the warning: while a kill asks `[y/N]`, the
//! selection's bar warms from the ink towards the terminal's own red, as it
//! warms from the terminal's foreground with no theme (see
//! [`crate::ui::effects`]).
//!
//! What nvmux draws over a session is bytes rather than a frame. The notice a
//! switch puts up ([`crate::announce`]) and the name an attach hands across
//! ([`crate::handoff`]) write the ink into their pen
//! ([`crate::announce::pen`]), and dissolve from it
//! ([`crate::fade::Dissolve`]). The hint bar the prefix puts up
//! ([`crate::hint`]) is the picker's hint row, and like it stays dim.
//!
//! # What it never touches
//!
//! Neovim's screen. Whichever client draws a session draws it in the editor's
//! colours, and a session's own fade dissolves those ([`crate::shadow`]), not
//! the ink. Nor the first-run screen ([`crate::ui::setup`]), which is up before
//! there is a config to read the colour from.
//!
//! And `NO_COLOR` turns the theme off, as it does everything else that paints
//! a colour: [`ink`] is then `None`, and every screen is exactly what it is
//! with no theme at all.

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};

use crate::config::ThemeColor;
use crate::palette::Rgb;

/// The colour nvmux's own screens are drawn in, or `None` for the terminal's
/// own: no `[style] color`, or `NO_COLOR`.
pub fn ink() -> Option<Rgb> {
    configured(
        crate::config::get().style.color,
        std::env::var_os("NO_COLOR").is_some(),
    )
}

/// The gate itself, without the process globals [`ink`] reads. `NO_COLOR`
/// wins over the config, as it does for the fade: honouring it means setting
/// no colour whatever the file asks for.
fn configured(color: ThemeColor, no_color: bool) -> Option<Rgb> {
    match color {
        ThemeColor::Mono(ink) if !no_color => Some(ink),
        ThemeColor::Mono(_) | ThemeColor::Terminal => None,
    }
}

/// Draw everything on `buf` that is at full strength and has no colour of its
/// own in the ink, if there is one: the first thing done to a frame of any of
/// nvmux's screens once it is drawn.
pub fn paint(buf: &mut Buffer) {
    if let Some(ink) = ink() {
        paint_in(buf, ink);
    }
}

/// [`paint`], with the ink handed in rather than read.
///
/// Every cell whose foreground is the terminal's own and that is not dim,
/// blank or not: a blank shows none of it, and one that is reversed,
/// underlined or struck through shows it as its bar or its line. A dim cell
/// keeps the terminal's foreground, which the terminal dims, as it did before
/// there was a theme. A foreground something has already set is that colour
/// on purpose, and is left alone; so is every background, and every modifier.
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

    /// The config's colour, unless `NO_COLOR` says none at all; and none for
    /// the terminal's own whatever `NO_COLOR` says.
    #[test]
    fn the_ink_is_the_configured_colour_unless_no_color_says_none() {
        assert_eq!(configured(ThemeColor::Mono(INK), false), Some(INK));
        assert_eq!(configured(ThemeColor::Mono(INK), true), None);
        assert_eq!(configured(ThemeColor::Terminal, false), None);
        assert_eq!(configured(ThemeColor::Terminal, true), None);
    }

    /// No config is ever installed under test, so the screens are the
    /// terminal's own, and every other test's expectations stand.
    #[test]
    fn under_test_there_is_no_ink() {
        assert_eq!(ink(), None);
    }

    /// The pass colours what is at full strength and had no colour, and
    /// nothing else: what is dim stays the terminal's, a colour already set
    /// stays, no background is set, no modifier is touched.
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

    /// A themed screen dissolves out of the colours it is drawn in: what is at
    /// full strength from the ink, what is dim from the terminal's foreground
    /// — so neither changes colour on the fade's first frame.
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
