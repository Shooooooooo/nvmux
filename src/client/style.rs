//! Highlights, and the colours a cell is finally drawn in.
//!
//! Neovim defines every highlight once (`hl_attr_define`) in two forms — its
//! GUI colours and its cterm ones — and from then on a cell names it by id.
//! Which form the client draws is `'termguicolors'`, exactly as Neovim's own
//! TUI decides it, so a session looks the same in this client as in that one:
//! RGB with it set, the terminal's 256 colours without. A colour neither the
//! highlight nor `default_colors_set` gives is the terminal's own, drawn as
//! the terminal's default rather than as a guess at it — which is what keeps
//! a transparent background transparent.
//!
//! # Colours as they look
//!
//! An animation mixes colours, and mixing needs real ones at both ends. So a
//! [`Style`] can also be read as it *looks* ([`Colors::visual_fg`],
//! [`Colors::visual_bg`]): its reverse applied, an index looked up, and a
//! default made into the terminal's own colour from the palette nvmux asked
//! the terminal for (see [`crate::palette`]). A cell an animation has touched
//! is drawn in those, which is the one place this client draws a colour
//! Neovim did not name.
//!
//! # Blending
//!
//! With `ext_multigrid` a float's `'winblend'` is the client's to draw: the
//! float's cells mixed with what is under them. [`blend_over`] and
//! [`blend_through`] are Neovim's own rules for it (`hl_blend_attrs` in
//! `highlight.c`), ratios and all, so a blended float looks as it would in
//! the TUI.

use crate::palette::{Palette, Rgb};

/// How a highlight underlines, if it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Underline {
    #[default]
    None,
    Line,
    Curl,
    Double,
    Dotted,
    Dashed,
}

/// One of a highlight's two forms, as `hl_attr_define` sends it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Attrs {
    /// `0xRRGGBB` in the GUI form, a 256-colour index in the cterm one.
    pub fg: Option<u32>,
    pub bg: Option<u32>,
    pub sp: Option<u32>,
    pub reverse: bool,
    pub bold: bool,
    pub italic: bool,
    pub strikethrough: bool,
    pub altfont: bool,
    pub underline: Underline,
    /// `'winblend'` / `'pumblend'`, 0 to 100.
    pub blend: u8,
    /// A hyperlink's target. Kept, not yet drawn.
    pub url: Option<Box<str>>,
}

/// The editor's default colours: `Normal`'s, where it has them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DefaultColors {
    pub rgb_fg: Option<u32>,
    pub rgb_bg: Option<u32>,
    pub rgb_sp: Option<u32>,
    /// Zero-based, already turned from the protocol's one-based form.
    pub cterm_fg: Option<u8>,
    pub cterm_bg: Option<u8>,
}

/// A colour as the client draws it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    /// The terminal's own.
    #[default]
    Default,
    Rgb(Rgb),
    Index(u8),
}

/// Everything a cell is drawn with but its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub sp: Color,
    pub reverse: bool,
    pub bold: bool,
    pub italic: bool,
    pub strikethrough: bool,
    pub altfont: bool,
    pub underline: Underline,
}

impl Style {
    /// The style with its colours swapped in, reverse spent: what a reversed
    /// cell looks like, written down so it can be mixed.
    fn unreversed(self, colors: &Colors) -> Style {
        Style {
            fg: Color::Rgb(colors.visual_fg(&self)),
            bg: Color::Rgb(colors.visual_bg(&self)),
            reverse: false,
            ..self
        }
    }
}

/// Mix `a` and `b` the way Neovim does (`rgb_blend`): `ratio` percent of `a`.
pub fn mix(ratio: u8, a: Rgb, b: Rgb) -> Rgb {
    let r = u32::from(ratio.min(100));
    let ch = |x: u8, y: u8| ((r * u32::from(x) + (100 - r) * u32::from(y)) / 100) as u8;
    Rgb(ch(a.0, b.0), ch(a.1, b.1), ch(a.2, b.2))
}

/// `0xRRGGBB` as a colour.
pub fn rgb(n: u32) -> Rgb {
    Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8)
}

/// What resolves a style's colours to the ones it shows: whether RGB is in
/// force, the editor's defaults, and the terminal's own palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colors {
    pub rgb: bool,
    pub defaults: DefaultColors,
    pub term: Palette,
}

impl Colors {
    /// A highlight resolved for drawing: its own colours, then the editor's
    /// defaults, then the terminal's — in the form `'termguicolors'` says.
    pub fn style(&self, rgb: &Attrs, cterm: &Attrs) -> Style {
        let d = &self.defaults;
        let (a, fg, bg, sp) = if self.rgb {
            let pick = |c: Option<u32>, dflt: Option<u32>| {
                c.or(dflt)
                    .map_or(Color::Default, |n| Color::Rgb(self::rgb(n)))
            };
            (
                rgb,
                pick(rgb.fg, d.rgb_fg),
                pick(rgb.bg, d.rgb_bg),
                pick(rgb.sp, d.rgb_sp),
            )
        } else {
            let pick = |c: Option<u32>, dflt: Option<u8>| {
                c.and_then(|n| u8::try_from(n).ok())
                    .or(dflt)
                    .map_or(Color::Default, Color::Index)
            };
            (
                cterm,
                pick(cterm.fg, d.cterm_fg),
                pick(cterm.bg, d.cterm_bg),
                Color::Default,
            )
        };
        Style {
            fg,
            bg,
            sp,
            reverse: a.reverse,
            bold: a.bold,
            italic: a.italic,
            strikethrough: a.strikethrough,
            altfont: a.altfont,
            underline: a.underline,
        }
    }

    /// The style a cell with no highlight is drawn in.
    pub fn default_style(&self) -> Style {
        self.style(&Attrs::default(), &Attrs::default())
    }

    /// A colour as it shows, `fg` saying which default stands in for none.
    pub fn resolve(&self, c: Color, fg: bool) -> Rgb {
        match c {
            Color::Rgb(rgb) => rgb,
            Color::Index(n) => self.term.index(n),
            Color::Default if fg => self.term.fg,
            Color::Default => self.term.bg,
        }
    }

    /// The colour a cell's text shows in, reverse and all.
    pub fn visual_fg(&self, s: &Style) -> Rgb {
        if s.reverse {
            self.resolve(s.bg, false)
        } else {
            self.resolve(s.fg, true)
        }
    }

    /// The colour a cell's background shows in, reverse and all.
    pub fn visual_bg(&self, s: &Style) -> Rgb {
        if s.reverse {
            self.resolve(s.fg, true)
        } else {
            self.resolve(s.bg, false)
        }
    }

    /// The colour an underline shows in: its own, or the text's.
    pub fn visual_sp(&self, s: &Style) -> Rgb {
        match s.sp {
            Color::Default => self.visual_fg(s),
            c => self.resolve(c, true),
        }
    }
}

/// A blended float's cell over what is under it, where the float's cell has
/// text of its own: the float's style, its text colour moved half the blend's
/// way towards the text under it and its background the whole way.
/// `hl_blend_attrs` without `through`.
pub fn blend_over(colors: &Colors, back: &Style, front: &Style, blend: u8) -> Style {
    let b = back.unreversed(colors);
    let f = front.unreversed(colors);
    let mut out = f;
    if blend >= 50 {
        out.bold |= b.bold;
        out.italic |= b.italic;
        out.strikethrough |= b.strikethrough;
        if out.underline == Underline::None {
            out.underline = b.underline;
        }
    }
    let rgb = |c: Color| match c {
        Color::Rgb(rgb) => rgb,
        _ => unreachable!("unreversed styles are all RGB"),
    };
    out.fg = Color::Rgb(mix(blend / 2, rgb(b.fg), rgb(f.fg)));
    out.sp = if out.underline != Underline::None {
        Color::Rgb(mix(blend / 2, rgb(b.bg), colors.visual_sp(front)))
    } else {
        Color::Default
    };
    out.bg = mixed_bg(colors, back, front, blend);
    out
}

/// A blended float's blank cell over what is under it: the text under it shows
/// through, in its own style, its colours moved the blend's way towards the
/// float's background. `hl_blend_attrs` with `through`.
pub fn blend_through(colors: &Colors, back: &Style, front: &Style, blend: u8) -> Style {
    let float_bg = colors.visual_bg(front);
    let b = back.unreversed(colors);
    let rgb = |c: Color| match c {
        Color::Rgb(rgb) => rgb,
        _ => unreachable!("unreversed styles are all RGB"),
    };
    let mut out = b;
    out.fg = Color::Rgb(mix(blend, rgb(b.fg), float_bg));
    out.sp = if b.underline != Underline::None {
        Color::Rgb(mix(blend, colors.visual_sp(back), float_bg))
    } else {
        Color::Default
    };
    out.bg = mixed_bg(colors, back, front, blend);
    out
}

/// Both backgrounds the terminal's own stays the terminal's own; otherwise
/// they mix the blend's way.
fn mixed_bg(colors: &Colors, back: &Style, front: &Style, blend: u8) -> Color {
    let plain = |s: &Style| !s.reverse && s.bg == Color::Default;
    if plain(back) && plain(front) {
        return Color::Default;
    }
    Color::Rgb(mix(blend, colors.visual_bg(back), colors.visual_bg(front)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> Palette {
        Palette {
            fg: Rgb(200, 200, 200),
            bg: Rgb(10, 10, 10),
            ansi: [Rgb(1, 1, 1); 16],
        }
    }

    fn colors(rgb: bool) -> Colors {
        Colors {
            rgb,
            defaults: DefaultColors {
                rgb_fg: Some(0xe0e2ea),
                rgb_bg: None,
                rgb_sp: None,
                cterm_fg: None,
                cterm_bg: Some(4),
            },
            term: palette(),
        }
    }

    #[test]
    fn mixing_weighs_the_first_colour_by_the_ratio() {
        assert_eq!(mix(100, Rgb(200, 0, 0), Rgb(0, 0, 200)), Rgb(200, 0, 0));
        assert_eq!(mix(0, Rgb(200, 0, 0), Rgb(0, 0, 200)), Rgb(0, 0, 200));
        assert_eq!(mix(30, Rgb(100, 0, 0), Rgb(0, 0, 100)), Rgb(30, 0, 70));
    }

    /// With `'termguicolors'` a highlight's own GUI colour wins, then the
    /// editor's default, then the terminal's: the last left as the terminal's
    /// default, never a guess at it.
    #[test]
    fn rgb_colours_fall_back_to_the_defaults_and_then_the_terminal() {
        let c = colors(true);
        let s = c.style(
            &Attrs {
                fg: Some(0x102030),
                ..Attrs::default()
            },
            &Attrs::default(),
        );
        assert_eq!(s.fg, Color::Rgb(Rgb(0x10, 0x20, 0x30)));
        assert_eq!(s.bg, Color::Default);
        assert_eq!(c.default_style().fg, Color::Rgb(Rgb(0xe0, 0xe2, 0xea)));
    }

    /// Without it, the cterm form is drawn, attributes and all.
    #[test]
    fn without_termguicolors_the_cterm_form_is_drawn() {
        let c = colors(false);
        let s = c.style(
            &Attrs {
                fg: Some(0x102030),
                bold: true,
                ..Attrs::default()
            },
            &Attrs {
                fg: Some(10),
                underline: Underline::Line,
                ..Attrs::default()
            },
        );
        assert_eq!(s.fg, Color::Index(10));
        assert_eq!(s.bg, Color::Index(4), "the default cterm background");
        assert!(
            !s.bold,
            "the GUI form's attributes are not the cterm form's"
        );
        assert_eq!(s.underline, Underline::Line);
    }

    /// As it looks: reversed, indexed and defaulted colours all come out as
    /// real ones.
    #[test]
    fn a_style_reads_as_it_looks() {
        let c = colors(true);
        let s = Style {
            fg: Color::Default,
            bg: Color::Rgb(Rgb(1, 2, 3)),
            reverse: true,
            ..Style::default()
        };
        assert_eq!(c.visual_fg(&s), Rgb(1, 2, 3));
        assert_eq!(c.visual_bg(&s), palette().fg);
        let plain = Style::default();
        assert_eq!(c.visual_fg(&plain), palette().fg);
        assert_eq!(c.visual_bg(&plain), palette().bg);
    }

    /// Neovim's numbers, for a float at `'winblend'` 30: a blank lets the text
    /// under it through, tinted 70% of the way to the float's background.
    #[test]
    fn a_blank_in_a_blended_float_lets_the_text_through() {
        let c = colors(true);
        let back = Style {
            fg: Color::Rgb(Rgb(100, 100, 100)),
            bg: Color::Rgb(Rgb(0, 0, 0)),
            bold: true,
            ..Style::default()
        };
        let front = Style {
            fg: Color::Rgb(Rgb(255, 255, 255)),
            bg: Color::Rgb(Rgb(200, 0, 0)),
            ..Style::default()
        };
        let out = blend_through(&c, &back, &front, 30);
        assert_eq!(out.fg, Color::Rgb(Rgb(170, 30, 30)));
        assert_eq!(out.bg, Color::Rgb(Rgb(140, 0, 0)));
        assert!(out.bold, "the text under keeps its own look");
    }

    /// Text of the float's own keeps the float's style, its colour moved half
    /// the blend towards what is under it, and attributes merge from 50 up.
    #[test]
    fn text_in_a_blended_float_keeps_its_own_style() {
        let c = colors(true);
        let back = Style {
            fg: Color::Rgb(Rgb(0, 0, 0)),
            bg: Color::Rgb(Rgb(0, 0, 100)),
            italic: true,
            ..Style::default()
        };
        let front = Style {
            fg: Color::Rgb(Rgb(200, 200, 200)),
            bg: Color::Rgb(Rgb(100, 0, 0)),
            ..Style::default()
        };
        let out = blend_over(&c, &back, &front, 40);
        assert_eq!(out.fg, Color::Rgb(Rgb(160, 160, 160)));
        assert_eq!(out.bg, Color::Rgb(Rgb(60, 0, 40)));
        assert!(!out.italic);
        assert!(blend_over(&c, &back, &front, 60).italic);
    }

    /// Where neither side has a background of its own, the terminal's shows
    /// through still — transparent stays transparent.
    #[test]
    fn two_default_backgrounds_stay_default() {
        let c = colors(true);
        let out = blend_through(&c, &Style::default(), &Style::default(), 50);
        assert_eq!(out.bg, Color::Default);
    }
}
