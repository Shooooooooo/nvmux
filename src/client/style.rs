//! Highlights and the colours a cell is drawn in.
//!
//! Neovim defines each highlight once (`hl_attr_define`) in GUI and cterm
//! forms. As in Neovim's TUI, `'termguicolors'` picks RGB or the terminal's
//! 256 colours. A colour set by neither the highlight nor
//! `default_colors_set` is drawn as the terminal default, not a guess, so a
//! transparent background stays transparent.
//!
//! Animations mix colours, so a [`Style`] can also be resolved to the RGB it
//! looks like ([`Colors::visual_fg`], [`Colors::visual_bg`]) using the
//! terminal's palette (see [`crate::palette`]). Animated cells are the only
//! ones drawn in colours Neovim did not name.
//!
//! With `ext_multigrid` the client draws `'winblend'` itself;
//! [`blend_over`] and [`blend_through`] follow Neovim's `hl_blend_attrs`
//! (`highlight.c`) exactly.

use crate::palette::{Palette, Rgb};

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
    /// `0xRRGGBB` in the GUI form, a 256-colour index in the cterm form.
    pub fg: Option<u32>,
    pub bg: Option<u32>,
    pub sp: Option<u32>,
    pub reverse: bool,
    pub bold: bool,
    pub italic: bool,
    pub strikethrough: bool,
    pub altfont: bool,
    pub dim: bool,
    pub blink: bool,
    pub conceal: bool,
    pub overline: bool,
    pub underline: Underline,
    /// `'winblend'` / `'pumblend'`, 0 to 100.
    pub blend: u8,
    /// Hyperlink target; Neovim sends it with the GUI form only.
    pub url: Option<Box<str>>,
}

/// The editor's default colours (`Normal`'s, where set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DefaultColors {
    pub rgb_fg: Option<u32>,
    pub rgb_bg: Option<u32>,
    pub rgb_sp: Option<u32>,
    /// Zero-based; the protocol sends them one-based.
    pub cterm_fg: Option<u8>,
    pub cterm_bg: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    /// The terminal's default.
    #[default]
    Default,
    Rgb(Rgb),
    Index(u8),
}

/// Everything a cell is drawn with except its text.
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
    pub dim: bool,
    pub blink: bool,
    pub conceal: bool,
    pub overline: bool,
    pub underline: Underline,
    /// Hyperlink index into `super::model::Model::links`, 0 for none. An index
    /// keeps `Style` `Copy` and cheap to compare.
    pub link: u32,
}

impl Style {
    /// The style with reverse applied and colours resolved to RGB, for mixing.
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

pub fn rgb(n: u32) -> Rgb {
    Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8)
}

/// Everything needed to resolve a style's colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colors {
    /// `'termguicolors'`.
    pub rgb: bool,
    pub defaults: DefaultColors,
    pub term: Palette,
}

impl Colors {
    /// Resolve a highlight in the form `'termguicolors'` selects: its own
    /// colours, else the editor's defaults, else the terminal's.
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
            dim: a.dim,
            blink: a.blink,
            conceal: a.conceal,
            overline: a.overline,
            underline: a.underline,
            link: 0,
        }
    }

    pub fn default_style(&self) -> Style {
        self.style(&Attrs::default(), &Attrs::default())
    }

    /// The RGB a colour shows as; `fg` selects which terminal default
    /// `Color::Default` means.
    pub fn resolve(&self, c: Color, fg: bool) -> Rgb {
        match c {
            Color::Rgb(rgb) => rgb,
            Color::Index(n) => self.term.index(n),
            Color::Default if fg => self.term.fg,
            Color::Default => self.term.bg,
        }
    }

    /// Text colour as shown, reverse applied.
    pub fn visual_fg(&self, s: &Style) -> Rgb {
        if s.reverse {
            self.resolve(s.bg, false)
        } else {
            self.resolve(s.fg, true)
        }
    }

    /// Background colour as shown, reverse applied.
    pub fn visual_bg(&self, s: &Style) -> Rgb {
        if s.reverse {
            self.resolve(s.fg, true)
        } else {
            self.resolve(s.bg, false)
        }
    }

    /// Underline colour: its own, else the text's.
    pub fn visual_sp(&self, s: &Style) -> Rgb {
        match s.sp {
            Color::Default => self.visual_fg(s),
            c => self.resolve(c, true),
        }
    }
}

/// A blended float cell that has text: the float's style, with fg mixed at
/// `blend / 2` and bg at `blend` towards what is under it. `hl_blend_attrs`
/// without `through`.
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

/// A blank blended float cell: the text under it shows through in its own
/// style, mixed towards the float's background. `hl_blend_attrs` with
/// `through`.
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

/// Two terminal-default backgrounds stay default; otherwise they mix.
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

    /// Expected values are Neovim's for `'winblend'` 30.
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

    /// Attributes from below merge only at blend 50 and up.
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

    #[test]
    fn two_default_backgrounds_stay_default() {
        let c = colors(true);
        let out = blend_through(&c, &Style::default(), &Style::default(), 50);
        assert_eq!(out.bg, Color::Default);
    }
}
