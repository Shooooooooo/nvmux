//! The `[theme]` table: a colour, or none, for each kind of thing nvmux draws
//! on its own screens.
//!
//! With no theme those screens set no colour at all (see [`crate::ui`]):
//! every distinction is a modifier — reversed and bold for the selection, dim
//! for what is not deciding anything, underlined for what a filter matched —
//! so they are drawn in the terminal's own palette, whatever it is. A theme
//! asks for colour one role at a time, and a role left at `"default"` is
//! drawn exactly as it was. The default theme is every role left there, which
//! is why every screen's no-colour test still holds at the defaults.
//!
//! # Roles, not widgets
//!
//! Six, for the things the screens already tell apart: the selection
//! ([`Theme::select`]), what is not deciding anything ([`Theme::muted`]), a
//! filter's matched letters ([`Theme::matched`]), and the three kinds of note
//! a session can have ([`Theme::busy`], [`Theme::notify`], [`Theme::fail`]),
//! the last of which also says what went wrong. A role is the same colour on
//! every screen it is on, so the picker's bar and the prompt's menu are one
//! colour, as they are one modifier now.
//!
//! # A colour, never a background
//!
//! No role sets a cell's background, so the screens still sit on the
//! terminal's own, transparent or not, and nothing [`crate::ui::effects`]
//! takes for the terminal's background stops being it. The selection keeps
//! its reverse and takes the colour as its *foreground*: reversed, that is
//! the colour the bar is painted in, and the text on it is the terminal's own
//! background. A muted colour replaces dim rather than joining it: how far a
//! terminal dims is its own guess, and the colour is already the answer —
//! one that is not reset by the same SGR as bold, either.
//!
//! # Whose shade
//!
//! A name — `"blue"`, `"bright-black"` — is one of the terminal's own sixteen,
//! so the terminal's palette decides the shade, as it does for everything else
//! drawn through nvmux. `"#rrggbb"` is that colour exactly.
//!
//! A fade needs the shade as a number, and takes it from the terminal's answer
//! to OSC 4 ([`crate::palette`]). A terminal that does not answer one — tmux,
//! Terminal.app — leaves xterm's sixteen standing in, so there a named colour
//! can start or end a fade a shade off, as a session's own ANSI colours
//! already do; `"#rrggbb"` cannot.
//!
//! # NO_COLOR
//!
//! Wins over the file, as it does over `[effects.fade] enabled`: under it
//! [`current`] is the default theme whatever `[theme]` says, and nothing
//! nvmux draws sets a colour.

use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;

use crate::notes::Kind;
use crate::palette::{Palette, Rgb};

/// The terminal's eight colour names, in the order of their SGR numbers.
/// Each is also a bright one, [`BRIGHT`] before it, eight further on.
const NAMES: [&str; 8] = [
    "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
];

/// What makes a name one of the bright eight.
const BRIGHT: &str = "bright-";

/// One role's colour, as the file gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Colour {
    /// No colour: the role as nvmux draws it without a theme.
    #[default]
    Default,
    /// One of the terminal's own sixteen, by its index, 0–15. What shade that
    /// is, the terminal says.
    Ansi(u8),
    /// Exactly this colour.
    Rgb(Rgb),
}

impl Colour {
    /// A colour as the file spells it: `"default"`, one of the eight names or
    /// a `bright-` one, or `"#rrggbb"`. Case and surrounding space do not
    /// matter, as they do not for `[keys] prefix`.
    pub fn parse(s: &str) -> Result<Colour, String> {
        let lower = s.trim().to_ascii_lowercase();
        if lower == "default" {
            return Ok(Colour::Default);
        }
        if let Some(hex) = lower.strip_prefix('#') {
            return hex_colour(hex).ok_or_else(|| {
                format!("colour {s:?} must be # and six hex digits, like \"#7aa2f7\"")
            });
        }
        let (base, name) = match lower.strip_prefix(BRIGHT) {
            Some(name) => (8, name),
            None => (0, lower.as_str()),
        };
        NAMES
            .iter()
            .position(|n| *n == name)
            .map(|i| Colour::Ansi(base + i as u8))
            .ok_or_else(|| {
                format!(
                    "colour {s:?} must be \"default\", one of the terminal's own — \
                     {} — or one of them bright, like \"bright-black\", or \"#rrggbb\"",
                    NAMES.join(", ")
                )
            })
    }

    /// The value as the file spells it.
    pub fn name(self) -> String {
        match self {
            Colour::Default => "default".to_string(),
            Colour::Ansi(n) => {
                let name = NAMES[usize::from(n % 8)];
                if n >= 8 {
                    format!("{BRIGHT}{name}")
                } else {
                    name.to_string()
                }
            }
            Colour::Rgb(Rgb(r, g, b)) => format!("#{r:02x}{g:02x}{b:02x}"),
        }
    }

    /// The colour as a ratatui cell's, or `None` for the default, which sets
    /// nothing.
    pub fn to_color(self) -> Option<Color> {
        match self {
            Colour::Default => None,
            Colour::Ansi(n) => Some(Color::Indexed(n)),
            Colour::Rgb(Rgb(r, g, b)) => Some(Color::Rgb(r, g, b)),
        }
    }

    /// The colour as the terminal draws it, or `None` for the default — which
    /// is whatever the caller's own default is.
    pub fn rgb(self, palette: &Palette) -> Option<Rgb> {
        match self {
            Colour::Default => None,
            Colour::Ansi(n) => Some(palette.index(n)),
            Colour::Rgb(c) => Some(c),
        }
    }

    /// The SGR parameters that make it the foreground, for the rows written
    /// over a live session as bytes rather than drawn by ratatui: `38;5;n` and
    /// `38;2;r;g;b`, which is how crossterm writes the picker's own cells, so
    /// a colour is the same bytes on every screen.
    fn sgr(self) -> Option<String> {
        match self {
            Colour::Default => None,
            Colour::Ansi(n) => Some(format!("38;5;{n}")),
            Colour::Rgb(Rgb(r, g, b)) => Some(format!("38;2;{r};{g};{b}")),
        }
    }
}

/// Six hex digits as a colour. Checked digit by digit first, because
/// `from_str_radix` would also take a sign.
fn hex_colour(hex: &str) -> Option<Colour> {
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let n = u32::from_str_radix(hex, 16).ok()?;
    Some(Colour::Rgb(Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8)))
}

/// A colour comes in as a human string; [`Colour::parse`] is the one parser,
/// and its message is folded into serde's error, which TOML then reports with
/// the line it is on — the way `[keys] prefix` is read.
impl<'de> Deserialize<'de> for Colour {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Colour::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// `[theme]`: what colour each role is drawn in, each `"default"` — no colour
/// — unless the file says otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Theme {
    /// The selection: the bar on the selected row, in the picker and in the
    /// create prompt's directory menu; every cursor; and the notice a switch
    /// puts up over the session it lands in.
    pub select: Colour,
    /// What is not deciding anything: the hint rows and the hint bar the
    /// prefix puts up, the session numbers and the back mark, the prompt's
    /// labels and suggestions, the attaching screen's status. Dim with no
    /// colour; given one, that colour in place of dim.
    pub muted: Colour,
    /// The letters a filter matched: underlined either way, and on the
    /// selection's bar only underlined, so the bar stays one colour.
    #[serde(rename = "match")]
    pub matched: Colour,
    /// A busy session's sign — the spinner, or the gauge — and the attaching
    /// screen's spinner, which is the same one.
    pub busy: Colour,
    /// The sign of a session a program in it sent a notification from.
    pub notify: Colour,
    /// The sign of a session whose progress failed, and whatever nvmux's
    /// screens say went wrong: an attach, a listing or a kill that failed, a
    /// name the prompt turned down.
    pub fail: Colour,
}

impl Theme {
    /// The selection's bar: reversed and bold, as it always is, with the
    /// colour as the cell's foreground — which is what a reversed cell
    /// paints its background in.
    pub fn bar(&self) -> Style {
        fg(
            Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD),
            self.select,
        )
    }

    /// A one-cell cursor, and a note in the selection's bar: reversed and not
    /// bold, the crisper form, in the selection's colour.
    pub fn reversed(&self) -> Style {
        fg(Style::new().add_modifier(Modifier::REVERSED), self.select)
    }

    /// A cursor drawn as a glyph, `▋`, rather than as a reversed cell: in the
    /// selection's colour, or plain.
    pub fn caret(&self) -> Style {
        fg(Style::new(), self.select)
    }

    /// What is not deciding anything: dim, or the muted colour instead.
    pub fn muted(&self) -> Style {
        match self.muted.to_color() {
            Some(c) => Style::new().fg(c),
            None => Style::new().add_modifier(Modifier::DIM),
        }
    }

    /// The letters a filter matched, off the selection's bar.
    pub fn matched(&self) -> Style {
        fg(
            Style::new().add_modifier(Modifier::UNDERLINED),
            self.matched,
        )
    }

    /// A session's note — its sign, or the note itself in its row — in the
    /// colour of its kind, or muted, as every note is without one.
    pub fn note(&self, kind: Kind) -> Style {
        let colour = match kind {
            Kind::Busy(_) => self.busy,
            Kind::Notified => self.notify,
            Kind::Failed => self.fail,
        };
        colour
            .to_color()
            .map_or_else(|| self.muted(), |c| Style::new().fg(c))
    }

    /// Something that went wrong, said on a screen: plain, or in the fail
    /// colour.
    pub fn error(&self) -> Style {
        fg(Style::new(), self.fail)
    }

    /// The SGR the hint bar is written under over a live session: a reset,
    /// then dim — or the muted colour instead of dim.
    pub fn hint_sgr(&self) -> String {
        reset_then(self.muted, Some("2"))
    }

    /// The SGR the attach notice is written under at rest: a reset alone, so
    /// it is drawn in the terminal's own text colour — or the selection's.
    pub fn notice_sgr(&self) -> String {
        reset_then(self.select, None)
    }
}

/// `style` in `colour`, if it is one.
fn fg(style: Style, colour: Colour) -> Style {
    match colour.to_color() {
        Some(c) => style.fg(c),
        None => style,
    }
}

/// A reset, then `colour` as the foreground — or `otherwise`, for none.
///
/// One CSI either way: the reset is what puts the terminal's own background
/// back under a row written over a session, and whatever else the row is
/// drawn in rides along on it rather than following it (see
/// [`crate::announce::placed`]).
fn reset_then(colour: Colour, otherwise: Option<&str>) -> String {
    match colour.sgr().as_deref().or(otherwise) {
        Some(rest) => format!("\x1b[0;{rest}m"),
        None => "\x1b[0m".to_string(),
    }
}

/// The theme nvmux's screens are drawn in: `[theme]`, unless `NO_COLOR` says
/// no colour at all.
///
/// Read when a screen opens, never by the drawing itself: the first-run
/// screen draws before there are any settings to read (see
/// [`crate::ui::setup`]), and reading them would fix them at the defaults.
pub fn current() -> Theme {
    effective(
        crate::config::get().theme,
        std::env::var_os("NO_COLOR").is_some(),
    )
}

/// The gate itself, apart from the process globals [`current`] reads, so the
/// precedence can be tested.
fn effective(theme: Theme, no_color: bool) -> Theme {
    if no_color {
        Theme::default()
    } else {
        theme
    }
}

/// The colour a cell's text is on the terminal: the terminal's own
/// foreground for a cell that sets none, and the colour it sets otherwise.
///
/// What a fade or a passing effect sets out from, so a cell a theme drew in a
/// colour leaves from that colour, rather than from the terminal's text
/// colour in one jump on its first frame. With no theme no cell sets one, and
/// this is the terminal's foreground, as it always was.
pub fn ink(palette: &Palette, fg: Color) -> Rgb {
    let ansi = |n: u8| palette.index(n);
    match fg {
        Color::Reset => palette.fg,
        Color::Rgb(r, g, b) => Rgb(r, g, b),
        Color::Indexed(n) => palette.index(n),
        Color::Black => ansi(0),
        Color::Red => ansi(1),
        Color::Green => ansi(2),
        Color::Yellow => ansi(3),
        Color::Blue => ansi(4),
        Color::Magenta => ansi(5),
        Color::Cyan => ansi(6),
        Color::Gray => ansi(7),
        Color::DarkGray => ansi(8),
        Color::LightRed => ansi(9),
        Color::LightGreen => ansi(10),
        Color::LightYellow => ansi(11),
        Color::LightBlue => ansi(12),
        Color::LightMagenta => ansi(13),
        Color::LightCyan => ansi(14),
        Color::White => ansi(15),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::XTERM_ANSI;
    use crate::test_support::{palette, theme};

    #[test]
    fn every_spelling_of_a_colour_parses() {
        for (spelt, colour) in [
            ("default", Colour::Default),
            ("black", Colour::Ansi(0)),
            ("red", Colour::Ansi(1)),
            ("green", Colour::Ansi(2)),
            ("yellow", Colour::Ansi(3)),
            ("blue", Colour::Ansi(4)),
            ("magenta", Colour::Ansi(5)),
            ("cyan", Colour::Ansi(6)),
            ("white", Colour::Ansi(7)),
            ("bright-black", Colour::Ansi(8)),
            ("bright-red", Colour::Ansi(9)),
            ("bright-white", Colour::Ansi(15)),
            ("#7aa2f7", Colour::Rgb(Rgb(0x7a, 0xa2, 0xf7))),
            ("#000000", Colour::Rgb(Rgb(0, 0, 0))),
            // Case and surrounding space, as for the prefix.
            ("  Blue ", Colour::Ansi(4)),
            ("Bright-Black", Colour::Ansi(8)),
            ("#7AA2F7", Colour::Rgb(Rgb(0x7a, 0xa2, 0xf7))),
            ("DEFAULT", Colour::Default),
        ] {
            assert_eq!(Colour::parse(spelt), Ok(colour), "{spelt:?}");
        }
    }

    #[test]
    fn a_colour_nvmux_cannot_name_is_refused() {
        for spelt in [
            "",
            "blu",
            "bright black",
            "brightblack",
            "bright-",
            "grey",
            "15",
            "#fff",
            "#12345g",
            "#1234567",
            "#+abcde",
            "7aa2f7",
            "rgb:7a/a2/f7",
        ] {
            let err = Colour::parse(spelt).expect_err(spelt);
            assert!(err.contains(&format!("{spelt:?}")), "{spelt:?}: {err}");
        }
        // Each says what would have been taken.
        assert!(Colour::parse("blu").unwrap_err().contains("bright-black"));
        assert!(Colour::parse("#fff")
            .unwrap_err()
            .contains("six hex digits"));
    }

    #[test]
    fn a_colours_name_is_how_the_file_spells_it() {
        for colour in [
            Colour::Default,
            Colour::Ansi(0),
            Colour::Ansi(4),
            Colour::Ansi(8),
            Colour::Ansi(15),
            Colour::Rgb(Rgb(0x7a, 0xa2, 0xf7)),
        ] {
            assert_eq!(Colour::parse(&colour.name()), Ok(colour), "{colour:?}");
        }
        assert_eq!(Colour::Ansi(8).name(), "bright-black");
        assert_eq!(Colour::Rgb(Rgb(0x7a, 0xa2, 0xf7)).name(), "#7aa2f7");
    }

    #[test]
    fn a_colour_is_the_bytes_crossterm_would_write_for_it() {
        assert_eq!(Colour::Default.sgr(), None);
        assert_eq!(Colour::Ansi(4).sgr().as_deref(), Some("38;5;4"));
        assert_eq!(Colour::Ansi(8).sgr().as_deref(), Some("38;5;8"));
        assert_eq!(
            Colour::Rgb(Rgb(1, 2, 3)).sgr().as_deref(),
            Some("38;2;1;2;3")
        );
        assert_eq!(Colour::Default.to_color(), None);
        assert_eq!(Colour::Ansi(4).to_color(), Some(Color::Indexed(4)));
        assert_eq!(
            Colour::Rgb(Rgb(1, 2, 3)).to_color(),
            Some(Color::Rgb(1, 2, 3))
        );
    }

    #[test]
    fn a_name_is_the_terminals_shade_and_a_hex_is_exact() {
        let p = Palette {
            ansi: XTERM_ANSI,
            ..palette()
        };
        assert_eq!(Colour::Default.rgb(&p), None);
        assert_eq!(Colour::Ansi(4).rgb(&p), Some(XTERM_ANSI[4]));
        assert_eq!(Colour::Rgb(Rgb(1, 2, 3)).rgb(&p), Some(Rgb(1, 2, 3)));
    }

    /// With no theme, every role is the modifier nvmux has always drawn it
    /// with, and the overlays' bytes are the ones they always wrote.
    #[test]
    fn the_default_theme_is_the_look_nvmux_always_had() {
        let t = Theme::default();
        let only = |m: Modifier| Style::new().add_modifier(m);
        assert_eq!(t.bar(), only(Modifier::REVERSED | Modifier::BOLD));
        assert_eq!(t.reversed(), only(Modifier::REVERSED));
        assert_eq!(t.caret(), Style::new());
        assert_eq!(t.muted(), only(Modifier::DIM));
        assert_eq!(t.matched(), only(Modifier::UNDERLINED));
        for kind in [
            Kind::Busy(None),
            Kind::Busy(Some(42)),
            Kind::Notified,
            Kind::Failed,
        ] {
            assert_eq!(t.note(kind), only(Modifier::DIM), "{kind:?}");
        }
        assert_eq!(t.error(), Style::new());
        assert_eq!(t.hint_sgr(), "\x1b[0;2m");
        assert_eq!(t.notice_sgr(), "\x1b[0m");
    }

    #[test]
    fn a_role_given_a_colour_keeps_its_modifiers_but_dim() {
        let t = theme();
        let select = t.select.to_color().expect("a select colour");
        assert_eq!(
            t.bar(),
            Style::new()
                .fg(select)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD)
        );
        assert_eq!(
            t.reversed(),
            Style::new().fg(select).add_modifier(Modifier::REVERSED)
        );
        assert_eq!(t.caret(), Style::new().fg(select));
        // Muted is the colour in place of dim, not as well as it.
        assert_eq!(t.muted(), Style::new().fg(Color::Indexed(8)));
        assert_eq!(
            t.matched(),
            Style::new()
                .fg(Color::Indexed(3))
                .add_modifier(Modifier::UNDERLINED)
        );
        assert_eq!(
            t.note(Kind::Busy(None)),
            Style::new().fg(Color::Indexed(11))
        );
        assert_eq!(
            t.note(Kind::Busy(Some(5))),
            Style::new().fg(Color::Indexed(11))
        );
        assert_eq!(t.note(Kind::Notified), Style::new().fg(Color::Indexed(5)));
        let fail = t.fail.to_color().expect("a fail colour");
        assert_eq!(t.note(Kind::Failed), Style::new().fg(fail));
        assert_eq!(t.error(), Style::new().fg(fail));
        // And none of them is a background.
        for style in [
            t.bar(),
            t.reversed(),
            t.caret(),
            t.muted(),
            t.matched(),
            t.error(),
        ] {
            assert_eq!(style.bg, None, "{style:?}");
        }
    }

    /// A kind of note with no colour of its own is muted, as every note is
    /// with no theme — whatever muted is.
    #[test]
    fn a_note_with_no_colour_of_its_own_is_muted() {
        let t = Theme {
            muted: Colour::Ansi(8),
            ..Theme::default()
        };
        for kind in [Kind::Busy(None), Kind::Notified, Kind::Failed] {
            assert_eq!(t.note(kind), t.muted(), "{kind:?}");
        }
    }

    #[test]
    fn the_overlays_take_their_colour_on_the_reset() {
        let t = theme();
        assert_eq!(t.hint_sgr(), "\x1b[0;38;5;8m");
        assert_eq!(t.notice_sgr(), "\x1b[0;38;2;122;162;247m");
        let named = Theme {
            select: Colour::Ansi(4),
            ..Theme::default()
        };
        assert_eq!(named.notice_sgr(), "\x1b[0;38;5;4m");
        assert_eq!(named.hint_sgr(), "\x1b[0;2m", "muted is still dim");
    }

    #[test]
    fn no_color_is_no_theme() {
        assert_eq!(effective(theme(), true), Theme::default());
        assert_eq!(effective(theme(), false), theme());
    }

    #[test]
    fn a_cells_ink_is_the_colour_it_shows() {
        let p = Palette {
            ansi: XTERM_ANSI,
            ..palette()
        };
        assert_eq!(ink(&p, Color::Reset), p.fg);
        assert_eq!(ink(&p, Color::Rgb(1, 2, 3)), Rgb(1, 2, 3));
        assert_eq!(ink(&p, Color::Indexed(4)), XTERM_ANSI[4]);
        assert_eq!(ink(&p, Color::Indexed(232)), p.index(232));
        assert_eq!(ink(&p, Color::Blue), XTERM_ANSI[4]);
        assert_eq!(ink(&p, Color::DarkGray), XTERM_ANSI[8]);
        assert_eq!(ink(&p, Color::White), XTERM_ANSI[15]);
    }
}
