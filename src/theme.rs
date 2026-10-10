//! The colour nvmux draws its own screens in.
//!
//! nvmux's screens set no colour of their own (see [`crate::ui`]): their text
//! is the terminal's foreground, shaded with dim, bold and reverse, on the
//! terminal's background. `[theme] color` names one colour to draw them in
//! instead (see [`crate::config::ThemeSettings`]), and this module is where
//! that colour — the *ink* — goes on.
//!
//! # One colour, in shades
//!
//! The theme is monochrome, and it gets its shades the way the screens always
//! have: from their modifiers, not from a palette of its own. [`paint`] gives
//! every cell of a finished frame that has no colour of its own the ink, and
//! leaves the modifiers as they were. So a name is the ink, a number or a hint
//! is the ink dimmed, the selection's bar is the ink as a background with the
//! name standing out of it in the terminal's background colour, and an
//! underline or a line through a name is the ink because the text is. Nothing
//! sets a background: the terminal's own stays under everything, transparent
//! or not.
//!
//! A post-pass for the reason the fade is one ([`crate::fade::apply`]): the
//! screens' own drawing stays colourless, and its tests go on saying so. It is
//! the last thing done to a frame — after the effects and the fade have
//! painted — and colours only what they left without a colour.
//!
//! # What fades from it
//!
//! The fade and the picker's effects paint colours of their own, and each one
//! starts its text from the colour the text is drawn in. Themed, that is the
//! ink rather than the terminal's foreground, so they are handed [`palette`]:
//! the terminal's palette with the ink in place of its foreground, and a
//! themed screen dissolves out of the colour it is drawn in rather than
//! jumping to the terminal's on the first frame. In place of the sixteen ANSI
//! colours too, which nvmux's screens reach for in one place — the red a row's
//! bar warms towards while a kill asks `[y/N]` — and a monochrome screen has
//! no red: themed, that bar stays the ink, and the line through the name is
//! the warning. The background stays the terminal's own, being what
//! everything dissolves into.
//!
//! What nvmux draws over a session — the hint bar ([`crate::hint`]), the
//! notice a switch puts up ([`crate::announce`]) and the name an attach hands
//! across ([`crate::handoff`]) — is bytes rather than a frame, so each writes
//! the ink into its own pen ([`crate::announce::pen`]), and the two that
//! dissolve dissolve from it ([`crate::fade::Dissolve`]).
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
use ratatui::style::Color;

use crate::config::ThemeColor;
use crate::palette::{Palette, Rgb};

/// The colour nvmux's own screens are drawn in, or `None` for the terminal's
/// own: no `[theme] color`, or `NO_COLOR`.
pub fn ink() -> Option<Rgb> {
    configured(
        crate::config::get().theme.color,
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

/// The palette nvmux's own screens are drawn in: the terminal's, with the ink
/// in place of every colour the screens could draw text in — the foreground
/// and the sixteen — when there is one. What the fade and the effects
/// interpolate with over those screens, so they start from the colour each
/// is drawn in.
pub fn palette(terminal: &Palette) -> Palette {
    inked(terminal, ink())
}

/// [`palette`], with the ink handed in rather than read.
pub(crate) fn inked(terminal: &Palette, ink: Option<Rgb>) -> Palette {
    match ink {
        Some(ink) => Palette {
            fg: ink,
            bg: terminal.bg,
            ansi: [ink; 16],
        },
        None => *terminal,
    }
}

/// Draw everything on `buf` that has no colour of its own in the ink, if there
/// is one: the last thing done to a frame of any of nvmux's screens.
pub fn paint(buf: &mut Buffer) {
    if let Some(ink) = ink() {
        paint_in(buf, ink);
    }
}

/// [`paint`], with the ink handed in rather than read.
///
/// Every cell whose foreground is the terminal's own, blank or not: a blank
/// shows none of it, and one that is reversed, underlined or struck through
/// shows it as its bar or its line. A foreground something has already set —
/// a fade's frame, an effect — is that colour on purpose, and is left alone;
/// so is every background, and every modifier.
pub(crate) fn paint_in(buf: &mut Buffer, ink: Rgb) {
    let colour = Color::Rgb(ink.0, ink.1, ink.2);
    for cell in &mut buf.content {
        if cell.fg == Color::Reset {
            cell.set_fg(colour);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::palette as terminal;
    use ratatui::layout::Rect;
    use ratatui::style::{Modifier, Style};

    const INK: Rgb = Rgb(0x7a, 0xa2, 0xf7);

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
        assert_eq!(palette(&terminal()), terminal());
    }

    /// Themed, every colour the screens could draw text in is the ink, and
    /// the background is still the terminal's: what everything dissolves into.
    #[test]
    fn a_themed_palette_has_one_colour_on_the_terminals_background() {
        let mut t = terminal();
        t.ansi[1] = Rgb(0xcd, 0, 0);
        let p = inked(&t, Some(INK));
        assert_eq!(p.fg, INK);
        assert_eq!(p.bg, t.bg);
        assert!(p.ansi.iter().all(|c| *c == INK), "{:?}", p.ansi);
        assert_eq!(inked(&t, None), t, "unthemed, the terminal's as it is");
    }

    /// The post-pass colours what had no colour, and nothing else: a colour
    /// already set stays, no background is set, no modifier is touched.
    #[test]
    fn only_what_has_no_colour_takes_the_ink() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 12, 2));
        let dim = Style::default().add_modifier(Modifier::DIM);
        let bar = Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD);
        buf.set_string(0, 0, "1", dim);
        buf.set_string(2, 0, "notes", bar);
        buf.set_string(0, 1, "set", Style::default().fg(Color::Rgb(1, 2, 3)));
        let before = buf.clone();

        paint_in(&mut buf, INK);

        let ink = Color::Rgb(INK.0, INK.1, INK.2);
        for (cell, was) in buf.content.iter().zip(&before.content) {
            assert_eq!(cell.bg, Color::Reset, "set a background: {cell:?}");
            assert_eq!(cell.modifier, was.modifier, "touched a modifier");
            assert_eq!(cell.symbol(), was.symbol());
            if was.fg == Color::Reset {
                assert_eq!(cell.fg, ink, "{cell:?}");
            } else {
                assert_eq!(cell.fg, was.fg, "repainted a colour: {cell:?}");
            }
        }
        assert!(buf[(0, 0)].modifier.contains(Modifier::DIM), "dim ink");
        assert!(
            buf[(2, 0)].modifier.contains(Modifier::REVERSED),
            "a bar of ink"
        );
    }

    /// A themed screen dissolves out of its ink, not out of the terminal's
    /// foreground: the fade's frame just short of the screen as drawn is all
    /// but the ink, and the frame that is the screen as drawn is the ink.
    #[test]
    fn a_themed_screen_dissolves_out_of_its_ink() {
        let draw = |buf: &mut Buffer| {
            buf.set_string(1, 0, "dotfiles", Style::default());
        };
        let p = inked(&terminal(), Some(INK));

        let mut drawn = Buffer::empty(Rect::new(0, 0, 12, 1));
        draw(&mut drawn);
        crate::fade::apply(&mut drawn, &p, 0.0);
        paint_in(&mut drawn, INK);
        assert_eq!(drawn[(1, 0)].fg, Color::Rgb(INK.0, INK.1, INK.2));

        let mut first = Buffer::empty(Rect::new(0, 0, 12, 1));
        draw(&mut first);
        crate::fade::apply(&mut first, &p, 0.01);
        paint_in(&mut first, INK);
        let near = INK.lerp(p.bg, 0.01);
        assert_eq!(first[(1, 0)].fg, Color::Rgb(near.0, near.1, near.2));
        assert_ne!(
            near,
            terminal().fg.lerp(p.bg, 0.01),
            "the fixture's ink must differ from its foreground to say anything"
        );
    }
}
