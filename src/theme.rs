//! `[theme] highlight`: one colour for the session the picker has highlighted,
//! and for the effects that come off its bar.
//!
//! The picker draws its selection as a reversed bar (see [`crate::ui::draw`]),
//! which shows the terminal's own foreground as the bar and its background as
//! the text. Under a theme the bar is still reversed — everything that knows a
//! bar by that, from the sparks that skip it to the rings that never land on
//! it, still does — and only its colours change: reversed, a cell's foreground
//! is what shows as the bar, so that is set to the highlight, and its
//! background is what shows as the text, so that is set when the text has to
//! be something other than the terminal's background to be read on it.
//!
//! Like the fade and the effects, it is a post-pass over a finished frame, so
//! `draw` and its tests never see it. With no `[theme] highlight`, or under
//! `NO_COLOR`, nothing is set, and a frame is byte for byte what it was before
//! there was a theme. The effects that come off the bar — the afterglow, the
//! glint, the strike and the sonar's rings — take their colours from the same
//! [`Highlight`] (see [`crate::ui::effects::Ink`]), and so do the stars,
//! sparks and dust a session being moved scatters beside the list (see
//! [`crate::ui::effects::particles`]).

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};

use crate::palette::{Palette, Rgb};

/// The picker's selection, under a theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Highlight {
    /// The bar: `[theme] highlight`.
    pub bar: Rgb,
    /// The text on it, or `None` for the terminal's own background — drawn as
    /// the default rather than as a colour, so a transparent terminal's stays
    /// transparent.
    pub text: Option<Rgb>,
}

impl Highlight {
    /// The bar `bar`, with whichever text reads on it.
    ///
    /// The terminal's own two colours when it said what they are, as an
    /// unthemed bar has them: its background, unless its foreground is the
    /// further from the bar in lightness. When it did not say, the only text
    /// certain to read is black on a light bar and white on a dark one.
    pub fn new(bar: Rgb, palette: Option<&Palette>) -> Self {
        let lightness = bar.lightness();
        let text = match palette {
            Some(p) => {
                let from_bg = (lightness - p.bg.lightness()).abs();
                let from_fg = (lightness - p.fg.lightness()).abs();
                (from_fg > from_bg).then_some(p.fg)
            }
            None if lightness >= 0.5 => Some(Rgb(0, 0, 0)),
            None => Some(Rgb(255, 255, 255)),
        };
        Self { bar, text }
    }

    /// Colour every reversed cell of `buf` that still has the terminal's own
    /// colours: the selection's bar, and whatever else of the picker is
    /// drawn as it — its landing, and the afterglow without a palette. A cell
    /// an effect has already painted, or taken the reverse off, is left as it
    /// is.
    pub fn paint(&self, buf: &mut Buffer) {
        for cell in &mut buf.content {
            if !cell.modifier.contains(Modifier::REVERSED) || cell.fg != Color::Reset {
                continue;
            }
            cell.set_fg(Color::Rgb(self.bar.0, self.bar.1, self.bar.2));
            if let (Some(Rgb(r, g, b)), Color::Reset) = (self.text, cell.bg) {
                cell.set_bg(Color::Rgb(r, g, b));
            }
        }
    }
}

/// Whether there is a highlight to draw: one is set and `NO_COLOR` is not.
pub fn wanted() -> bool {
    crate::config::get().theme.highlight.is_some() && std::env::var_os("NO_COLOR").is_none()
}

/// The highlight this run draws, if any: `[theme] highlight` unless
/// `NO_COLOR`, with its text chosen against the terminal's colours if they
/// are known.
pub fn highlight() -> Option<Highlight> {
    if std::env::var_os("NO_COLOR").is_some() {
        return None;
    }
    let bar = crate::config::get().theme.highlight?;
    Some(Highlight::new(bar, crate::palette::get()))
}

/// [`Highlight::paint`], or nothing without one.
pub fn paint(buf: &mut Buffer, highlight: Option<&Highlight>) {
    if let Some(h) = highlight {
        h.paint(buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::palette;
    use ratatui::layout::Rect;
    use ratatui::style::Style;

    const LIGHT: Rgb = Rgb(0x89, 0xb4, 0xfa);
    const DARK: Rgb = Rgb(0x31, 0x32, 0x44);

    #[test]
    fn only_the_reversed_cells_take_the_highlight() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 2));
        let bar = Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD);
        buf.set_string(0, 0, "ab ", bar);
        buf.set_string(0, 1, "cd", Style::default());
        buf[(3, 0)].set_style(bar);
        buf[(3, 0)].set_fg(Color::Rgb(1, 2, 3));
        let h = Highlight {
            bar: LIGHT,
            text: Some(Rgb(9, 9, 9)),
        };
        h.paint(&mut buf);
        for x in 0..3 {
            let cell = &buf[(x, 0)];
            assert_eq!(cell.fg, Color::Rgb(LIGHT.0, LIGHT.1, LIGHT.2), "{cell:?}");
            assert_eq!(cell.bg, Color::Rgb(9, 9, 9), "{cell:?}");
            assert!(cell.modifier.contains(Modifier::REVERSED), "still a bar");
        }
        assert_eq!(buf[(3, 0)].fg, Color::Rgb(1, 2, 3), "already painted");
        for x in 0..4 {
            assert_eq!(buf[(x, 1)].fg, Color::Reset, "a plain row");
            assert_eq!(buf[(x, 1)].bg, Color::Reset);
        }
    }

    /// Text that is the terminal's background stays the default, not a
    /// colour that matches it.
    #[test]
    fn the_terminals_background_as_text_is_left_the_default() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 2, 1));
        buf.set_string(
            0,
            0,
            "ab",
            Style::default().add_modifier(Modifier::REVERSED),
        );
        Highlight {
            bar: LIGHT,
            text: None,
        }
        .paint(&mut buf);
        assert_eq!(buf[(0, 0)].fg, Color::Rgb(LIGHT.0, LIGHT.1, LIGHT.2));
        assert_eq!(buf[(0, 0)].bg, Color::Reset);
    }

    #[test]
    fn no_highlight_sets_nothing() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 2, 1));
        buf.set_string(
            0,
            0,
            "ab",
            Style::default().add_modifier(Modifier::REVERSED),
        );
        let before = buf.clone();
        paint(&mut buf, None);
        assert_eq!(buf, before);
    }

    /// On a dark terminal a light bar keeps the terminal's background as its
    /// text, as the unthemed bar does, and a dark bar takes its foreground;
    /// not knowing the terminal's colours, black or white.
    #[test]
    fn the_text_is_whichever_reads_on_the_bar() {
        let p = palette();
        assert_eq!(Highlight::new(LIGHT, Some(&p)).text, None);
        assert_eq!(Highlight::new(DARK, Some(&p)).text, Some(p.fg));
        assert_eq!(Highlight::new(LIGHT, None).text, Some(Rgb(0, 0, 0)));
        assert_eq!(Highlight::new(DARK, None).text, Some(Rgb(255, 255, 255)));
    }

    /// No test installs a config, so the compiled default is what every
    /// picker test draws with: no highlight.
    #[test]
    fn there_is_no_highlight_by_default() {
        assert_eq!(crate::config::ThemeSettings::default().highlight, None);
        assert_eq!(highlight(), None);
        assert!(!wanted());
    }
}
