//! The user's configuration file.
//!
//! Optional and override-only: an absent file, an empty file and an omitted
//! field all give the built-in behaviour, since each resolves through the
//! per-table `Default` impls, where every default is written once.
//!
//! # Strict
//!
//! [`deny_unknown_fields`] rejects unknown keys, unparsable values are hard
//! errors, and [`load`] reports them with the path. A file named by
//! `$NVMUX_CONFIG` must exist; the default paths may be absent.
//!
//! [`deny_unknown_fields`]: https://serde.rs/container-attrs.html#deny_unknown_fields
//!
//! # Loading
//!
//! `main` calls [`load`] once and passes the result to [`init`]; everything
//! else reads the process global through [`get`]. Without `init`, `get`
//! returns [`Settings::default`], so unit tests never read a developer's real
//! config.
//!
//! # Live
//!
//! `get` returns the current settings: the settings screen
//! ([`crate::ui::settings`]) changes them with [`apply`], and each change
//! bumps [`generation`] so a reader holding a copy can tell it is stale. The
//! file's contents are kept alongside ([`saved`]); the difference is what is
//! unsaved and what a save writes. Readers call `get` when they need a value
//! rather than holding one.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, SaveError};
use crate::palette::Rgb;

/// The whole configuration, one TOML table per field. `[effects]` has a
/// sub-table per effect: `fade`, `move`, `cursor`, `attach`, `kill`, `filter`
/// and `create` for nvmux's own screens; `smear`, `particles`, `scroll`,
/// `windows` and `blink` for nvmux's own client (`[client] ui = "nvmux"`, see
/// [`crate::client`]).
///
/// Not `Copy` (`[session] command` is a `String`); [`get`] hands out an `Arc`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub keys: KeySettings,
    pub session: SessionSettings,
    pub client: ClientSettings,
    pub style: StyleSettings,
    pub effects: EffectsSettings,
}

/// The prefix, the sequence timeout and the bindings (see [`crate::keys`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KeySettings {
    /// Written as `"C-Space"` / `"Ctrl-a"`, parsed to its control byte by
    /// [`crate::keys::parse_prefix`].
    #[serde(deserialize_with = "de_prefix")]
    pub prefix: u8,
    /// How long to wait for the byte after the prefix before taking it as a
    /// literal `<prefix>`, and for another digit of a half-typed session
    /// number.
    pub timeout_ms: u64,
    /// `[keys.bind]`: command name (as on the hint bar) to key, e.g.
    /// `detach = "x"`. Omitted commands keep their default
    /// ([`crate::keys::Keymap`]); keys parse with [`crate::keys::parse_key`].
    /// Two commands on one key are refused at startup.
    pub bind: crate::keys::Keymap,
}

/// What a new session launches when neither the prompt nor history says
/// otherwise.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionSettings {
    /// `{sock}` stands for the session's socket. Parsed and validated by
    /// [`crate::launch::Launch`].
    pub command: String,
    /// Retired: listings now always wait 0.25 s, not 3 s, on a session that
    /// cannot answer (see [`crate::rpc::QUICK_PROBE_TIMEOUT`]). Parsed so old
    /// files load; ignored.
    pub quick_list: Retired,
    /// Retired: sessions now always push news instead of being polled every
    /// 2 s (see [`crate::tell`]). Parsed so old files load; ignored.
    pub push_news: Retired,
}

/// The client that draws a session on the relayed pty (see [`crate::pty`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClientSettings {
    /// `"nvmux"` (default) is nvmux's own client (`nvmux --client`, see
    /// [`crate::client`]), which draws the editor itself and so can animate it
    /// (`[effects.smear]`, `particles`, `scroll`, `windows`, `blink`).
    /// `"nvim"` is Neovim's TUI, which negotiates terminal features itself.
    pub ui: Ui,
    /// Keep one client per session instead of one in all. Default on.
    ///
    /// Off, a switch retires the front client and starts another (fork,
    /// probe, wait for first paint). On, a client leaving the front is parked,
    /// still attached and read on its own thread; switching back redraws its
    /// last screen at once and asks the server to repaint (see
    /// [`crate::pty::Parked`]).
    ///
    /// Costs, for as long as nvmux runs:
    ///
    /// - A parked client is still a UI of its server: other UIs on the session
    ///   share its grid at the smaller size, and a second nvmux of your own
    ///   holds each session at the size its terminal had when it last left.
    /// - Switches no longer fire `UILeave`/`UIEnter`.
    /// - Each visited session keeps an idle `nvim` client (~1.5 MB), a thread,
    ///   a screen copy and its ssh forward; one that keeps redrawing keeps
    ///   sending it frames, over the link if remote.
    /// - No limit: one per session visited.
    /// - The restored screen lacks undercurl, underline colour, strikethrough,
    ///   blink, conceal and overline (see [`crate::shadow`]) until the
    ///   server's repaint lands, a round trip or two later.
    pub per_session: bool,
    /// Start a session's client on its first visit instead of all at startup.
    ///
    /// Off (default), a client is started for every session before the picker
    /// appears and held hidden until its first visit (see
    /// [`crate::pty::Attachment::start_ahead`]), which then waits for nothing:
    /// its output, terminal queries included, is replayed and the answers go
    /// back to it. On, the first attach pays a fork, a probe and a first
    /// paint. A session created after startup starts on its first visit either
    /// way, unless a `ui` change comes first.
    ///
    /// Background clients drop with their SSH link; the front session
    /// reattaches when it returns. Off, the others are then restarted ahead on
    /// a thread once the front has painted; on, each reattaches on its next
    /// visit (fork, forward, probe, first paint). A `ui` change retires every
    /// client; off, the other sessions' clients are then restarted as the new
    /// kind behind the front one.
    ///
    /// Costs of off:
    ///
    /// - Every session has a client (with `per_session`'s costs) from startup.
    /// - Startup pays a listing and, per session, a fork and an ssh forward
    ///   before the picker is drawn.
    /// - Every session is probed at startup and after a `ui` change, which
    ///   ends a hit-enter prompt in each, as a visit would (see
    ///   `pty::probe_on`).
    /// - A client that writes more than [`crate::pty::AHEAD_MAX`] before its
    ///   first visit is dropped and started on that visit instead.
    /// - Right after a dropped link returns, a fork, forward and screen resend
    ///   per session, sharing a slow link with the front session's frames.
    ///
    /// With `per_session` off too, a client started ahead is retired when it
    /// first leaves the front.
    pub lazy: bool,
    /// Retired: now governed by `lazy`. Parsed so old files load; ignored.
    #[serde(skip_serializing)]
    pub revive: Retired,
    /// Retired: unseen sessions are now always drawn at most once a second
    /// (see [`crate::card::Command::Away`]). Parsed so old files load; ignored.
    #[serde(skip_serializing)]
    pub throttle: Retired,
    /// Scroll ahead of a slow link: scrolling keys (`<C-e>`, `<C-d>`, `<C-f>`,
    /// `zz`, `zl` and similar, in Normal or Insert mode) and the mouse wheel
    /// scroll the window before Neovim hears of it. nvmux's own client only
    /// (see [`crate::client::predict`]).
    ///
    /// `"auto"` (default for all three `predict_*`): on for sessions over ssh,
    /// off locally. `"on"` includes local sessions; `"off"` waits for Neovim.
    /// Even a 20-30 ms round trip is felt with keys held or typed fast; a local
    /// one is a fraction of a millisecond, so Neovim's frame beats any
    /// prediction.
    ///
    /// Costs: uncovered rows lack signs and virtual text until Neovim's frame,
    /// a round trip later, and get tree-sitter and buffer highlights but not
    /// `:syntax` colours; the client's in-editor agent sends the lines around
    /// each view, with colours, as it moves (more the slower the link).
    pub predict_scrolls: Predict,
    /// Move the cursor ahead of a slow link: Normal-mode motions are drawn
    /// where Neovim will put the cursor, only when the client can work that
    /// out exactly, and checked against what Neovim reports (see
    /// [`crate::client::predict::Sync`]); a mismatch snaps back at once.
    ///
    /// Predicted, with counts: `h`, `l`, `0`, `^`, `$`, `g_`, `|`; `j`, `k`,
    /// `+`, `-`, `_`, `<CR>` (and LazyVim's `j`/`k` mappings); `w`, `b`, `e`,
    /// `ge`, `W`, `B`, `E`, `gE` (per `'iskeyword'`); `f`, `t`, `F`, `T` unless
    /// mapped or remapped by `'iminsert'` `:lmap`s; `}`, `{`; `H`, `M`, `L`;
    /// `gg`, `G`. They update `'relativenumber'` and `'cursorline'`, and past
    /// `'scrolloff'` scroll the view as Neovim would, within the lines the
    /// agent keeps, but never to reveal a line with virtual text or a sign.
    /// With `predict_scrolls`, scrolls and motions chain from each other.
    /// Line-changing motions in a window with a `'statuscolumn'` (as LazyVim
    /// has) are left to Neovim.
    ///
    /// Costs (shared with `predict_typing`): ~40 bytes back per key for the
    /// cursor position; as the view moves, a few bytes per run of lines with
    /// virtual text or signs, scanned while Neovim waits for a key (at most
    /// 1.5 ms); and ~40 bytes per fence the client sends after its keys.
    pub predict_motions: Predict,
    /// Type ahead of a slow link: in Insert mode, inserted characters, `<BS>`,
    /// `<C-w>` and `<C-u>` (stopping where Insert began, as Neovim does), plus
    /// `i`, `a`, `A`, `I` and the `<Esc>` after them, are drawn as Neovim will,
    /// with the mode's cursor shape, and checked as `predict_motions` are.
    ///
    /// Not predicted: keys a mapping or abbreviation may expand, keys past
    /// `'textwidth'`, or keys that reindent. In buffers whose plugins react to
    /// typing (or to entering and leaving Insert mode), keys are predicted
    /// only once enough have been seen to do nothing extra; one that later does
    /// more (e.g. a trailing-blank trimmer) is drawn as the plain key,
    /// corrected a round trip later, and its kind must prove itself again.
    ///
    /// Costs: those of `predict_motions`, plus an edit per Insert-mode key
    /// (not per pause) carrying the cursor's whole line: ~100 bytes on a short
    /// line, up to four times the window width on a long one (800 bytes for
    /// 800 characters, ~25 ms a key at 256 kbit/s); and a few hundred bytes on
    /// entering a buffer or Insert mode.
    pub predict_typing: Predict,
    /// Superseded by the three `predict_*` settings. `"auto"` and `"on"` are
    /// parsed and ignored (`"on"` acted as `"auto"` wherever the round trip was
    /// long enough to matter); `"off"` is refused with a message naming the
    /// replacements (see [`Superseded`]).
    #[serde(skip_serializing)]
    pub predict: Superseded,
    /// On a terminal wider than a window's text needs, centre the text in the
    /// window: lines wrap at the text width and the status line sits over it.
    /// nvmux's own client only (see `crate::client::wide`).
    ///
    /// `"off"` (default) uses the full window width, like Neovim's TUI.
    /// `"smart"` takes each window's text width from its buffer's
    /// `'textwidth'`, widened for unwrapped long lines (all of them with
    /// `'wrap'` off; with it on, those in Markdown tables or code blocks).
    /// `"fixed"` uses `window_width`.
    ///
    /// Terminal windows keep their full width (the program is told it), as do
    /// windows already centred, like start screens. Text past a line's end (a
    /// diagnostic, a blame) runs on into the margin.
    pub centre: Centre,
    /// Text width with `centre` on: every window's with `"fixed"`; with
    /// `"smart"`, for buffers without `'textwidth'` unless long lines need
    /// more. 0 means no limit, i.e. `centre = "off"`.
    pub window_width: u16,
    /// Retired: frames are now always written minimally (see
    /// [`crate::client::screen`]). Parsed so old files load; ignored.
    #[serde(skip_serializing)]
    pub lean_output: Retired,
    /// Retired: the terminal now always moves scrolled rows (see
    /// [`crate::client::region`]). Parsed so old files load; ignored.
    #[serde(skip_serializing)]
    pub scroll_regions: Retired,
    /// Retired: frames are now always built from changed rows where possible
    /// (see [`crate::client::App::frame`]). Parsed so old files load; ignored.
    #[serde(skip_serializing)]
    pub partial_frames: Retired,
}

/// A removed boolean switch: still parsed so files that set it load, then
/// ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Retired;

impl<'de> Deserialize<'de> for Retired {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        bool::deserialize(d).map(|_| Retired)
    }
}

impl Default for ClientSettings {
    fn default() -> Self {
        Self {
            ui: Ui::Nvmux,
            per_session: true,
            lazy: false,
            revive: Retired,
            throttle: Retired,
            predict_scrolls: Predict::Auto,
            predict_motions: Predict::Auto,
            predict_typing: Predict::Auto,
            predict: Superseded,
            centre: Centre::Off,
            window_width: 120,
            lean_output: Retired,
            scroll_regions: Retired,
            partial_frames: Retired,
        }
    }
}

impl ClientSettings {
    /// What a client predicts for a remote or local session.
    pub fn predictions(&self, remote: bool) -> Predictions {
        Predictions {
            scrolls: self.predict_scrolls.predicts(remote),
            motions: self.predict_motions.predicts(remote),
            typing: self.predict_typing.predicts(remote),
        }
    }

    pub fn predicts(&self, remote: bool) -> bool {
        self.predictions(remote).any()
    }
}

/// What a client draws ahead of Neovim for its session (see
/// [`ClientSettings::predictions`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Predictions {
    /// Scrolling keys and the mouse wheel.
    pub scrolls: bool,
    /// Normal-mode cursor motions.
    pub motions: bool,
    /// Insert-mode typing and the keys that enter and leave it.
    pub typing: bool,
}

impl Default for Predictions {
    /// Scrolls only, until the client learns its settings (see
    /// `crate::client::App::with_predictions`), so the agent sends only what
    /// scrolls need.
    fn default() -> Self {
        Self {
            scrolls: true,
            motions: false,
            typing: false,
        }
    }
}

impl Predictions {
    pub fn any(self) -> bool {
        self.scrolls || self.motions || self.typing
    }

    /// Whether the client follows the agent's per-key reports of what Neovim
    /// did (see [`crate::client::predict::Sync`]), since motions and typing
    /// chain from the previous key's cursor. Scrolls alone wait for Neovim.
    pub fn acks(self) -> bool {
        self.motions || self.typing
    }
}

/// `[client] predict_*`: which sessions a client predicts for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Predict {
    /// Sessions over ssh only.
    #[default]
    Auto,
    /// Every session, local too.
    On,
    /// None: each key waits for Neovim.
    Off,
}

impl Predict {
    /// In the settings screen's order.
    pub const ALL: [Predict; 3] = [Predict::Auto, Predict::On, Predict::Off];

    pub fn name(self) -> &'static str {
        match self {
            Predict::Auto => "auto",
            Predict::On => "on",
            Predict::Off => "off",
        }
    }

    pub fn predicts(self, remote: bool) -> bool {
        match self {
            Predict::Auto => remote,
            Predict::On => true,
            Predict::Off => false,
        }
    }
}

// Parsed from its words (see `de_way`); the old `true`/`false` form is
// refused with a message naming them.

impl<'de> Deserialize<'de> for Predict {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        de_way(deserializer, &Predict::ALL, Predict::name)
    }
}

/// `[client] predict`, still parsed (see [`ClientSettings::predict`]).
/// `"auto"` and `"on"` are ignored. `"off"` is refused, naming the settings
/// that turn every prediction off: ignoring it would turn predictions back
/// on, and mapping it onto all three could not round-trip, since the settings
/// screen omits a setting saved at its default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Superseded;

impl<'de> Deserialize<'de> for Superseded {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Was;

        impl serde::de::Visitor<'_> for Was {
            type Value = Superseded;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(
                    f,
                    "nothing: it is predict_scrolls, predict_motions and predict_typing now"
                )
            }

            fn visit_str<E: serde::de::Error>(self, word: &str) -> Result<Superseded, E> {
                match word {
                    "auto" | "on" => Ok(Superseded),
                    "off" => Err(E::custom(
                        "predict is three settings now: predict_scrolls, predict_motions and \
                         predict_typing, each \"auto\", \"on\" or \"off\"; for \"off\", set all three \
                         to \"off\"",
                    )),
                    _ => Err(E::invalid_value(serde::de::Unexpected::Str(word), &self)),
                }
            }
        }

        d.deserialize_str(Was)
    }
}

/// `[client] centre`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Centre {
    /// Full window width, as in Neovim's TUI.
    #[default]
    Off,
    /// Centred, as wide as the content wants.
    Smart,
    /// Centred, `window_width` wide.
    Fixed,
}

impl Centre {
    /// In the settings screen's order.
    pub const ALL: [Centre; 3] = [Centre::Off, Centre::Smart, Centre::Fixed];

    pub fn name(self) -> &'static str {
        match self {
            Centre::Off => "off",
            Centre::Smart => "smart",
            Centre::Fixed => "fixed",
        }
    }
}

// Parsed from its words (see `de_way`); anything else, the old boolean
// included, is refused with a message naming them.

impl<'de> Deserialize<'de> for Centre {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        de_way(deserializer, &Centre::ALL, Centre::name)
    }
}

/// Parse one of `all` by its `name`. Anything else is refused listing the
/// valid words (`expected "off", "smart" or "fixed"`), instead of TOML's
/// "wanted string or table" for a non-string like `true`.
fn de_way<'de, D, T>(
    deserializer: D,
    all: &'static [T],
    name: fn(T) -> &'static str,
) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Copy + 'static,
{
    struct Way<T: 'static> {
        all: &'static [T],
        name: fn(T) -> &'static str,
    }

    impl<T: Copy + 'static> serde::de::Visitor<'_> for Way<T> {
        type Value = T;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            for (i, way) in self.all.iter().enumerate() {
                let between = match i {
                    0 => "",
                    i if i + 1 == self.all.len() => " or ",
                    _ => ", ",
                };
                write!(f, "{between}{:?}", (self.name)(*way))?;
            }
            Ok(())
        }

        fn visit_str<E: serde::de::Error>(self, word: &str) -> Result<T, E> {
            self.all
                .iter()
                .copied()
                .find(|way| (self.name)(*way) == word)
                .ok_or_else(|| E::invalid_value(serde::de::Unexpected::Str(word), &self))
        }
    }

    deserializer.deserialize_str(Way { all, name })
}

/// What nvmux's own client draws with: `[client]`, `[effects]`, and whether
/// the sessions are remote, which predictions depend on (see
/// [`ClientSettings::predicts`]).
///
/// Passed in the client's environment at start ([`Drawing::to_env`]) and
/// resent on change (`card::Command::Settings`), not read from the file: the
/// file lacks unsaved settings-screen changes and where the sessions are
/// (the command line says that).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Drawing {
    pub client: ClientSettings,
    pub effects: EffectsSettings,
    /// Sessions are on another machine, over ssh (see [`remote`]).
    pub remote: bool,
}

impl Drawing {
    /// One-line JSON, for the client's environment.
    pub fn to_env(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Inverse of [`Drawing::to_env`].
    pub fn from_env(text: &str) -> Option<Drawing> {
        serde_json::from_str(text).ok()
    }

    /// These, with defaults for everything else.
    pub fn settings(&self) -> Settings {
        Settings {
            client: self.client,
            effects: self.effects,
            ..Settings::default()
        }
    }
}

impl Settings {
    pub fn drawing(&self) -> Drawing {
        Drawing {
            client: self.client,
            effects: self.effects,
            remote: remote(),
        }
    }
}

/// `[client] ui`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ui {
    /// `nvim --remote-ui`: Neovim's own TUI.
    Nvim,
    /// `nvmux --client`: nvmux's own, animated (see [`crate::client`]).
    #[default]
    Nvmux,
}

impl Ui {
    /// In the settings screen's order.
    pub const ALL: [Ui; 2] = [Ui::Nvmux, Ui::Nvim];

    pub fn name(self) -> &'static str {
        match self {
            Ui::Nvim => "nvim",
            Ui::Nvmux => "nvmux",
        }
    }
}

/// How nvmux's own screens look: their colour (see [`crate::theme`]) and
/// whether they float over the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StyleSettings {
    /// Colour of nvmux's own screens: picker, prompt, help, the wait for a
    /// session, and the name an attach shows over a session.
    ///
    /// `"terminal"` (default) sets no colour: terminal foreground and
    /// background, shaded with dim, bold and reverse. A colour (`"#rrggbb"` or
    /// `"#rgb"`) is used for the full-strength parts (names, typed text, the
    /// selection bar with its name in the background colour), and effects
    /// derive from it. Dim parts (session numbers, notes, key hints, the
    /// prefix hint bar) stay the terminal's, dimmed. The background is always
    /// the terminal's.
    ///
    /// Never applied to Neovim's screen or the first-run screen (shown before
    /// the config is read). `NO_COLOR` overrides it.
    pub color: ThemeColor,
    /// Whether the picker (`<prefix> Space`), settings (`<prefix> ,`) and
    /// help (`<prefix> ?`) float in a centred panel over the dimmed, live
    /// session (see `crate::pty::panel`); behind the picker, the session its
    /// cursor is on. Default on. Their hint row stays on the screen's last row
    /// either way.
    ///
    /// Off, each takes the whole terminal, the session dissolving out and
    /// back. The picker's `?` and `,` follow the picker. A terminal too small
    /// for a panel with session around it falls back to full screen. Changing
    /// it in the settings moves an open screen at once.
    pub float: bool,
}

impl Default for StyleSettings {
    fn default() -> Self {
        Self {
            color: ThemeColor::Terminal,
            float: true,
        }
    }
}

/// `[style] color`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeColor {
    /// No colour set: the terminal's own.
    #[default]
    Terminal,
    /// Shades of this colour.
    Mono(Rgb),
}

impl ThemeColor {
    /// `terminal` or `#rrggbb`.
    pub fn name(self) -> String {
        match self {
            ThemeColor::Terminal => "terminal".to_string(),
            ThemeColor::Mono(Rgb(r, g, b)) => format!("#{r:02x}{g:02x}{b:02x}"),
        }
    }

    /// Parse `terminal`, `#rrggbb` or `#rgb` (either case).
    ///
    /// Uses the terminal-reply parser ([`crate::palette`]), so `#f80` means
    /// `#ff8800` in both; its terminal-only forms (`rgb:ff/88/00`, twelve
    /// digits) are refused here.
    pub fn parse(s: &str) -> Result<ThemeColor, String> {
        if s == "terminal" {
            return Ok(ThemeColor::Terminal);
        }
        s.strip_prefix('#')
            .filter(|digits| matches!(digits.len(), 3 | 6))
            .and_then(|_| crate::palette::parse_colour(s))
            .map(ThemeColor::Mono)
            .ok_or_else(|| format!("color {s:?} must be \"terminal\" or a colour like \"#7aa2f7\""))
    }
}

/// Parsed by [`ThemeColor::parse`], its message folded into serde's error so
/// TOML reports it with the span, as `de_prefix` does.
impl<'de> Deserialize<'de> for ThemeColor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        ThemeColor::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// Animated effects: a master `enabled` plus a table per effect with its own
/// `enabled`. An effect runs only when both are on; the `*_enabled` methods
/// check both, so nothing reads one switch alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EffectsSettings {
    /// Master switch.
    pub enabled: bool,
    pub fade: FadeSettings,
    #[serde(rename = "move")]
    pub moving: MoveSettings,
    pub cursor: CursorSettings,
    pub attach: AttachSettings,
    pub kill: KillSettings,
    pub filter: FilterSettings,
    pub create: CreateSettings,
    pub smear: SmearSettings,
    pub particles: ParticleSettings,
    pub scroll: ScrollSettings,
    pub windows: WindowsSettings,
    pub blink: BlinkSettings,
}

impl EffectsSettings {
    /// `NO_COLOR` and the terminal are checked after this (see
    /// [`crate::fade::enabled`]).
    pub fn fade_enabled(&self) -> bool {
        self.enabled && self.fade.enabled
    }

    pub fn move_enabled(&self) -> bool {
        self.enabled && self.moving.enabled
    }

    /// Colour or modifier is then up to the terminal (see
    /// [`crate::ui::effects`]).
    pub fn cursor_enabled(&self) -> bool {
        self.enabled && self.cursor.enabled
    }

    /// Rides the fade's frames, so the fade decides the rest (see
    /// [`crate::handoff`]).
    pub fn attach_enabled(&self) -> bool {
        self.enabled && self.attach.enabled
    }

    pub fn kill_enabled(&self) -> bool {
        self.enabled && self.kill.enabled
    }

    pub fn filter_enabled(&self) -> bool {
        self.enabled && self.filter.enabled
    }

    /// Whether the picker closes onto the gap `c` opens or cuts away is up to
    /// the fade (see [`crate::ui::app::App::make_room`]).
    pub fn create_enabled(&self) -> bool {
        self.enabled && self.create.enabled
    }

    pub fn smear_enabled(&self) -> bool {
        self.enabled && self.smear.enabled
    }

    pub fn particles_enabled(&self) -> bool {
        self.enabled && self.particles.enabled
    }

    pub fn scroll_enabled(&self) -> bool {
        self.enabled && self.scroll.enabled
    }

    pub fn windows_enabled(&self) -> bool {
        self.enabled && self.windows.enabled
    }

    pub fn blink_enabled(&self) -> bool {
        self.enabled && self.blink.enabled
    }
}

// The client's effects are only switches (plus the particle mode); their
// look and timing live in `crate::client::anim`.

/// nvmux's own client: the cursor travels between cells with its leading
/// edge ahead, so long moves smear; Neovide's animated cursor (see
/// [`crate::client::anim::smear`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SmearSettings {
    /// Under `[effects] enabled`. Default off: the cursor jumps.
    pub enabled: bool,
}

/// nvmux's own client: particles off the moving cursor; Neovide's
/// `cursor_vfx_mode` (see [`crate::client::anim::vfx`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ParticleSettings {
    /// Under `[effects] enabled`. Default off, as in Neovide.
    pub enabled: bool,
    pub mode: ParticleMode,
}

/// `[effects.particles] mode`, spelt as Neovide's `cursor_vfx_mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ParticleMode {
    /// Particles coil off the cursor's path in a spiral.
    #[default]
    Railgun,
    /// Particles spray out behind the cursor.
    Torpedo,
    /// Particles drift down from the cursor's path.
    Pixiedust,
    /// A disc grows round where the cursor lands and fades.
    Sonicboom,
    /// A ring grows round where the cursor lands.
    Ripple,
    /// A square outline grows round where the cursor lands.
    Wireframe,
}

impl ParticleMode {
    /// In the settings screen's order.
    pub const ALL: [ParticleMode; 6] = [
        ParticleMode::Railgun,
        ParticleMode::Torpedo,
        ParticleMode::Pixiedust,
        ParticleMode::Sonicboom,
        ParticleMode::Ripple,
        ParticleMode::Wireframe,
    ];

    pub fn name(self) -> &'static str {
        match self {
            ParticleMode::Railgun => "railgun",
            ParticleMode::Torpedo => "torpedo",
            ParticleMode::Pixiedust => "pixiedust",
            ParticleMode::Sonicboom => "sonicboom",
            ParticleMode::Ripple => "ripple",
            ParticleMode::Wireframe => "wireframe",
        }
    }
}

/// nvmux's own client: scrolls slide the text a row at a time; Neovide's
/// smooth scrolling (see [`crate::client::anim::scroll`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScrollSettings {
    /// Under `[effects] enabled`. Off, the text jumps.
    pub enabled: bool,
}

/// nvmux's own client: windows move instead of jumping. Splits opening,
/// closing or resizing move like animate.nvim's window module; a window
/// changing buffer cross-fades (see [`crate::client::anim::layout`] and
/// [`crate::client::anim::switch`]); floats, the message area and rearranged
/// windows slide like Neovide's `position_animation_length` (see
/// [`crate::client::anim::motion`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WindowsSettings {
    /// Under `[effects] enabled`.
    pub enabled: bool,
}

/// nvmux's own client: a blinking block cursor fades out and in; Neovide's
/// `cursor_smooth_blink` (see [`crate::client::anim::blink`]). Bars and
/// underlines keep the terminal's blink.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BlinkSettings {
    /// Under `[effects] enabled`. Default off, as in Neovide. No effect when
    /// `'guicursor'` does not blink, or for a session out of sight, whose
    /// cursor holds still.
    pub enabled: bool,
}

/// The fade between screens (see [`crate::fade`]): each dissolves into the
/// terminal background and the next dissolves out of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FadeSettings {
    /// Under `[effects] enabled`. Forced off by `NO_COLOR` (it paints explicit
    /// colours) and by a terminal that does not answer the startup colour
    /// query (nothing to fade *to*); see [`crate::fade::enabled`].
    pub enabled: bool,
    /// Duration of a dissolve **out and in together**, split half each (see
    /// [`FadeSettings::one_way`]), since the whole change is what people
    /// time. A switch pays it once: its landing card comes with the session's
    /// screen (see [`crate::card`]) and the old announcement box is not drawn
    /// (see [`crate::announce`]). Must be 1..=`MAX_FADE_MS`.
    pub duration_ms: u64,
    /// Whether Neovim's screen dissolves too (in once its first paint
    /// settles, and out), not only nvmux's. Costs a running parse of the
    /// session's output while attached (see [`crate::shadow`]); off, sessions
    /// hard-cut. Kept clients (`[client] per_session`) parse anyway, so off
    /// saves the parse only with `per_session = false`.
    ///
    /// The hint bar restores the row it covered from that parse; off, it
    /// blanks the row until a repaint (see [`crate::hint`]).
    pub session: bool,
}

/// Moving a session in the picker: stars stream off its row while carried
/// (see [`crate::ui::starfield`]), rows it swaps with step aside, release the
/// bar and spark (see [`crate::ui::swap`]), and it lands with an impact (see
/// [`crate::ui::landing`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MoveSettings {
    /// Under `[effects] enabled`; one switch for the whole carry. Off, the
    /// carried session is still, swaps are jumps, there is no landing, and
    /// the picker redraws only on keys.
    ///
    /// `NO_COLOR` does not disable it: trail, sparks and landing are braille
    /// and modifiers, the sidestep moves whole cells, and a row releasing the
    /// bar uses a modifier instead of colour, as the cursor afterglow does.
    pub enabled: bool,
}

/// The row the cursor leaves fades from the reversed selection bar to plain
/// (see [`crate::ui::effects`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CursorSettings {
    /// Under `[effects] enabled`. Needs colour, as the fade does (a colour
    /// query answer and no `NO_COLOR`); otherwise the bar lingers reversed and
    /// dim before going. Duration: [`crate::ui::effects::AFTERGLOW`].
    pub enabled: bool,
}

/// Attach from the picker: the picker closes from top and bottom onto the
/// chosen row, the name stays up through the client's start, and the session
/// opens out of that line as the name dissolves (see [`crate::handoff`] and
/// [`crate::fade::Iris`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AttachSettings {
    /// Under `[effects] enabled`. Off, the picker dissolves whole.
    ///
    /// Adds no frames, only changes the fade's, so it needs the fade and
    /// `[effects.fade] session` on; with either off, or under `NO_COLOR`, it
    /// does nothing.
    pub enabled: bool,
}

/// A kill: while `[y/N]` asks, the name is struck through
/// ([`crate::ui::effects`]); once confirmed, the row is erased from its end
/// behind a cursor before the kill runs ([`crate::ui::backspace`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KillSettings {
    /// Under `[effects] enabled`. Off, the row is unchanged until the listing
    /// after the kill removes it.
    ///
    /// `NO_COLOR` does not disable it (strikethrough and characters); only the
    /// struck bar's warm tint is colour, and is dropped.
    pub enabled: bool,
}

/// Rows a filter keystroke drops fade out in place (see
/// [`crate::ui::effects`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FilterSettings {
    /// Under `[effects] enabled`. In colour if the terminal reported its
    /// colours, dim otherwise. Off, they vanish at once.
    pub enabled: bool,
}

/// `c` on the picker: rows below the cursor step down, the new session fades
/// into the gap, dim, with the name and number the prompt will offer, then
/// the picker closes onto that row and the prompt opens from its name field
/// (see [`crate::ui::app::App::make_room`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CreateSettings {
    /// Under `[effects] enabled`. Off, the prompt replaces the picker on the
    /// next frame.
    ///
    /// The gap and its row are layout and dim, so `NO_COLOR` does not affect
    /// them. The fade-in and the close and open around the prompt need the
    /// fade; without it the gap is held a moment, then the prompt cuts in.
    pub enabled: bool,
}

impl Default for EffectsSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            fade: FadeSettings::default(),
            moving: MoveSettings::default(),
            cursor: CursorSettings::default(),
            attach: AttachSettings::default(),
            kill: KillSettings::default(),
            filter: FilterSettings::default(),
            create: CreateSettings::default(),
            smear: SmearSettings::default(),
            particles: ParticleSettings::default(),
            scroll: ScrollSettings::default(),
            windows: WindowsSettings::default(),
            blink: BlinkSettings::default(),
        }
    }
}

// Defaults match Neovide where it has the same effect, except the smear,
// which is off (`SmearSettings` derives it).

impl Default for ParticleSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: ParticleMode::Railgun,
        }
    }
}

impl Default for ScrollSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Default for WindowsSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Default for MoveSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Default for CursorSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Default for AttachSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Default for KillSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Default for FilterSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Default for CreateSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

// The built-in defaults. With `#[serde(default)]` on the containers, an
// absent file, an empty file and an omitted field all land here.

impl Default for KeySettings {
    fn default() -> Self {
        Self {
            // `keys::PREFIX` is also read directly by the first-run screen and
            // `--help`, which run before any config loads.
            prefix: crate::keys::PREFIX,
            // Also how long a half-typed session number waits for another
            // digit (relay and picker). Every such wait is shown on screen (the
            // hint bar, the digits so far), so it can be long enough to read
            // the row and choose.
            timeout_ms: 1000,
            // Defaults from `keys::BINDINGS`.
            bind: crate::keys::Keymap::default(),
        }
    }
}

impl Default for SessionSettings {
    fn default() -> Self {
        Self {
            // A constant: the prompt may show it before any config is read.
            command: crate::launch::DEFAULT.to_string(),
            quick_list: Retired,
            push_news: Retired,
        }
    }
}

impl FadeSettings {
    /// Half of [`FadeSettings::duration_ms`], which covers both directions.
    /// Halved only here, so nothing reads the key as one direction by mistake.
    /// Never zero: `Duration` divides in nanoseconds, so 1 ms gives 500 µs,
    /// which is one frame (see [`crate::fade::Schedule::next`]).
    pub fn one_way(&self) -> Duration {
        Duration::from_millis(self.duration_ms) / 2
    }
}

impl Default for FadeSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            // 100 ms out, 100 ms back: a dissolve, not a flicker, yet one
            // movement.
            duration_ms: 200,
            session: true,
        }
    }
}

/// Parsed by [`crate::keys::parse_prefix`], its message folded into serde's
/// error so TOML reports it with the span.
fn de_prefix<'de, D>(deserializer: D) -> Result<u8, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    crate::keys::parse_prefix(&s).map_err(serde::de::Error::custom)
}

/// `[keys.bind]`: command names to key strings, applied over the defaults.
/// Clashes are checked by [`Settings::validate`].
impl<'de> Deserialize<'de> for crate::keys::Keymap {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let moved =
            std::collections::BTreeMap::<CommandName, CommandKey>::deserialize(deserializer)?;
        let mut keymap = crate::keys::Keymap::default();
        for (CommandName(name), CommandKey(key)) in moved {
            // `CommandName` accepts only known names, so this always applies.
            keymap.bind(&name, key);
        }
        Ok(keymap)
    }
}

/// A `[keys.bind]` name, one of [`crate::keys::NAMES`]. Others are refused as
/// `deny_unknown_fields` refuses any typo, with TOML pointing at the name.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct CommandName(String);

impl<'de> Deserialize<'de> for CommandName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let name = String::deserialize(deserializer)?;
        if crate::keys::NAMES.contains(&name.as_str()) {
            Ok(CommandName(name))
        } else {
            Err(serde::de::Error::unknown_field(&name, &crate::keys::NAMES))
        }
    }
}

/// Parsed by [`crate::keys::parse_key`], with errors reported as in
/// `de_prefix`.
struct CommandKey(u8);

impl<'de> Deserialize<'de> for CommandKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        crate::keys::parse_key(&s)
            .map(CommandKey)
            .map_err(serde::de::Error::custom)
    }
}

/// Ceiling for `keys.timeout_ms`: it becomes a `c_int` `poll` timeout, and an
/// absurd value is a hang.
pub(crate) const MAX_TIMEOUT_MS: u64 = 60_000;

/// Ceiling for `effects.fade.duration_ms`; every switch sits through it.
pub(crate) const MAX_FADE_MS: u64 = 2_000;

impl Settings {
    /// Rules serde cannot express; only values that would misbehave at
    /// runtime.
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.keys.timeout_ms == 0 {
            // A zero poll timeout spins the relay at full speed while a prefix
            // is armed, and a number could never be typed.
            return Err("keys.timeout_ms must be at least 1".into());
        }
        if self.keys.timeout_ms > MAX_TIMEOUT_MS {
            // Past `c_int` it wraps negative and `poll` blocks forever.
            return Err(format!("keys.timeout_ms must be at most {MAX_TIMEOUT_MS}"));
        }
        if let Some((first, second, key)) = self.keys.bind.clash() {
            // The second would be unreachable. Name both: either may be moved,
            // and one may be a default the file never mentions.
            return Err(format!(
                "keys.bind.{first} and keys.bind.{second} are both {:?}",
                crate::keys::key_label(key)
            ));
        }
        if self.effects.fade.duration_ms == 0 {
            // `enabled = false` turns the fade off; zero would be a schedule
            // with no frames.
            return Err("effects.fade.duration_ms must be at least 1".into());
        }
        if self.effects.fade.duration_ms > MAX_FADE_MS {
            // It feeds a `sleep`, on every screen a switch dissolves.
            return Err(format!(
                "effects.fade.duration_ms must be at most {MAX_FADE_MS}"
            ));
        }
        // Refused at startup, not at the prompt: the file is where it is fixed.
        // Each reason is a predicate, so it reads as a sentence after the key.
        if let Err(crate::error::SessionError::InvalidCommand { reason, .. }) =
            crate::launch::Launch::parse(&self.session.command)
        {
            return Err(format!("session.command {reason}"));
        }
        Ok(())
    }
}

/// Where the config file is, if anywhere.
#[derive(Debug, PartialEq, Eq)]
enum ConfigSource {
    /// Named by `$NVMUX_CONFIG`; must exist.
    Explicit(PathBuf),
    /// A default search path; may be absent.
    Default(PathBuf),
    /// Neither `$XDG_CONFIG_HOME` nor `$HOME` is set; use the defaults.
    None,
}

/// An empty value counts as unset. Also used by [`crate::state`].
pub(crate) fn nonempty(value: Option<&OsStr>) -> Option<&OsStr> {
    value.filter(|s| !s.is_empty())
}

/// Resolve the config path from the given environment (pure). Precedence:
/// `$NVMUX_CONFIG` (an exact path) > `$XDG_CONFIG_HOME/nvmux/` >
/// `$HOME/.config/nvmux/`.
fn resolve_config_path(
    nvmux_config: Option<&OsStr>,
    xdg_config_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> ConfigSource {
    if let Some(explicit) = nonempty(nvmux_config) {
        return ConfigSource::Explicit(PathBuf::from(explicit));
    }
    if let Some(xdg) = nonempty(xdg_config_home) {
        return ConfigSource::Default(Path::new(xdg).join("nvmux").join("config.toml"));
    }
    if let Some(home) = nonempty(home) {
        return ConfigSource::Default(
            Path::new(home)
                .join(".config")
                .join("nvmux")
                .join("config.toml"),
        );
    }
    ConfigSource::None
}

/// Load the configuration, or the defaults if there is no file. A missing
/// *default* file is fine; a missing `$NVMUX_CONFIG` file is an error. Every
/// failure carries the path.
pub fn load() -> Result<Settings, ConfigError> {
    let nvmux_config = std::env::var_os("NVMUX_CONFIG");
    let xdg = std::env::var_os("XDG_CONFIG_HOME");
    let home = std::env::var_os("HOME");

    match resolve_config_path(nvmux_config.as_deref(), xdg.as_deref(), home.as_deref()) {
        ConfigSource::None => Ok(Settings::default()),
        ConfigSource::Default(path) => match std::fs::read_to_string(&path) {
            Ok(contents) => parse(&path, &contents),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(error) => Err(ConfigError::Read { path, error }),
        },
        ConfigSource::Explicit(path) => match std::fs::read_to_string(&path) {
            Ok(contents) => parse(&path, &contents),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(ConfigError::ExplicitMissing(path))
            }
            Err(error) => Err(ConfigError::Read { path, error }),
        },
    }
}

/// Parse and validate the file contents, tagging every error with its path.
fn parse(path: &Path, contents: &str) -> Result<Settings, ConfigError> {
    let settings: Settings = toml::from_str(contents).map_err(|error| ConfigError::Parse {
        path: path.to_path_buf(),
        error,
    })?;
    settings
        .validate()
        .map_err(|message| ConfigError::Invalid {
            path: path.to_path_buf(),
            message,
        })?;
    Ok(settings)
}

// --- first run ------------------------------------------------------------

/// Where to write a first-run config, or `None` if nvmux must not offer one:
/// `$NVMUX_CONFIG` is set (the user owns it), no default path can be formed
/// (no `$HOME`/`$XDG_CONFIG_HOME`), or a config already exists.
pub fn first_run_target() -> Option<PathBuf> {
    let nvmux_config = std::env::var_os("NVMUX_CONFIG");
    let xdg = std::env::var_os("XDG_CONFIG_HOME");
    let home = std::env::var_os("HOME");
    match resolve_config_path(nvmux_config.as_deref(), xdg.as_deref(), home.as_deref()) {
        ConfigSource::Default(path) if !path.exists() => Some(path),
        _ => None,
    }
}

/// The first-run choice of prefix over the built-in defaults.
pub fn with_prefix(prefix: u8) -> Settings {
    Settings {
        keys: KeySettings {
            prefix,
            ..KeySettings::default()
        },
        ..Settings::default()
    }
}

/// The first-run config: a commented template listing every option at its
/// current default and setting only `[keys] prefix`, so other defaults keep
/// tracking the code. One line per option, as in the README; the full
/// description is on each field.
pub(crate) fn render_default_config(prefix: u8) -> String {
    let k = KeySettings::default();
    let s = SessionSettings::default();
    let c = ClientSettings::default();
    let t = StyleSettings::default();
    let e = EffectsSettings::default();
    let f = e.fade;
    // Quoted here, not with `{:?}` in the template, which would ignore the
    // padding widths that align each table's notes.
    let quoted = |s: &str| format!("{s:?}");
    // One line per command, generated from the table so none is missed: name,
    // key (as `key_label` spells it, which `parse_key` reads back) and help.
    let bind: String = crate::keys::BINDINGS
        .iter()
        .map(|b| {
            format!(
                "# {:<name$} = {:<7}   # {}\n",
                b.hint,
                quoted(&crate::keys::key_label(b.key)),
                b.help,
                name = command_width(),
            )
        })
        .collect();
    format!(
        "# nvmux configuration — created on first run.\n\
         #\n\
         # Uncomment and edit any line to override its default; delete this file\n\
         # to start over. See the README for what each option does.\n\
         \n\
         [keys]\n\
         prefix = {prefix:<12}   # Ctrl-Space, or a Ctrl-<letter> chord\n\
         # timeout_ms = {timeout:<6}   # how long a lone prefix or half-typed number waits\n\
         \n\
         [keys.bind]   # the key after the prefix: one character, \"Space\" or \"Enter\"\n\
         {bind}\
         \n\
         [session]\n\
         # command = {command}   # {{sock}} is required\n\
         \n\
         [client]\n\
         # ui              = {ui:<8}   # nvmux's own animated client, or \"nvim\" for Neovim's\n\
         # per_session     = {per_session:<8}   # keep each session's client; a switch back reuses it\n\
         # lazy            = {lazy:<8}   # start every client at launch, and after a dropped link; true waits for visits\n\
         # predict_scrolls = {predict_scrolls:<8}   # over ssh, scrolls show ahead of Neovim; \"on\" everywhere, or \"off\"\n\
         # predict_motions = {predict_motions:<8}   # over ssh, h, j, w, $ and their kind move the cursor ahead of Neovim\n\
         # predict_typing  = {predict_typing:<8}   # over ssh, what is typed in Insert mode shows ahead of Neovim\n\
         # centre          = {centre:<8}   # on a wide terminal, each window's text in its middle: \"smart\" or \"fixed\"\n\
         # window_width    = {window_width:<8}   # with centre on, how many columns a window's text gets; 0 for no limit\n\
         \n\
         [style]\n\
         # color = {style_color:<10}   # or \"#rrggbb\": nvmux's text and bars in that colour\n\
         # float = {style_float:<10}   # the picker, settings, help and prompts over the session; false takes the screen\n\
         \n\
         [effects]\n\
         # enabled = {effects_enabled}   # master switch: false turns every effect below off\n\
         \n\
         [effects.fade]\n\
         # enabled     = {fade_enabled:<4}   # dissolve between screens; NO_COLOR forces this off\n\
         # duration_ms = {fade_duration:<4}   # the whole dissolve, out and back\n\
         # session     = {fade_session:<4}   # Neovim's own screen dissolves too\n\
         \n\
         [effects.move]\n\
         # enabled = {move_enabled}   # stars off a session being moved, rows stepping aside\n\
         \n\
         [effects.cursor]\n\
         # enabled = {cursor_enabled}   # the row the cursor leaves fades back from the bar\n\
         \n\
         [effects.attach]\n\
         # enabled = {attach_enabled}   # the picker closes onto the name; the session opens from it\n\
         \n\
         [effects.kill]\n\
         # enabled = {kill_enabled}   # a line through the name at [y/N]; then the row is erased\n\
         \n\
         [effects.filter]\n\
         # enabled = {filter_enabled}   # rows a filter drops fade out before the list closes up\n\
         \n\
         [effects.create]\n\
         # enabled = {create_enabled}   # c opens a gap where the new session goes, then the prompt\n\
         \n\
         # The rest are for nvmux's own client, the default ui.\n\
         \n\
         [effects.smear]\n\
         # enabled = {smear_enabled}   # the cursor travels between cells rather than jumping\n\
         \n\
         [effects.particles]\n\
         # enabled = {particles_enabled:<9}   # what flies off the cursor as it moves\n\
         # mode    = {particles_mode:<9}   # or torpedo, pixiedust, sonicboom, ripple, wireframe\n\
         \n\
         [effects.scroll]\n\
         # enabled = {scroll_enabled}   # a scroll slides the text through its window\n\
         \n\
         [effects.windows]\n\
         # enabled = {windows_enabled}   # windows and floats move rather than jump\n\
         \n\
         [effects.blink]\n\
         # enabled = {blink_enabled}   # a blinking block cursor fades out and back in\n",
        prefix = quoted(&crate::keys::prefix_label(prefix)),
        timeout = k.timeout_ms,
        command = quoted(&s.command),
        per_session = c.per_session,
        lazy = c.lazy,
        predict_scrolls = quoted(c.predict_scrolls.name()),
        predict_motions = quoted(c.predict_motions.name()),
        predict_typing = quoted(c.predict_typing.name()),
        centre = quoted(c.centre.name()),
        window_width = c.window_width,
        style_color = quoted(&t.color.name()),
        style_float = t.float,
        fade_enabled = f.enabled,
        fade_duration = f.duration_ms,
        fade_session = f.session,
        effects_enabled = e.enabled,
        move_enabled = e.moving.enabled,
        cursor_enabled = e.cursor.enabled,
        attach_enabled = e.attach.enabled,
        kill_enabled = e.kill.enabled,
        filter_enabled = e.filter.enabled,
        create_enabled = e.create.enabled,
        ui = quoted(c.ui.name()),
        smear_enabled = e.smear.enabled,
        particles_enabled = e.particles.enabled,
        particles_mode = quoted(e.particles.mode.name()),
        scroll_enabled = e.scroll.enabled,
        windows_enabled = e.windows.enabled,
        blink_enabled = e.blink.enabled,
    )
}

/// Pad width for `[keys.bind]` names, so the keys line up.
fn command_width() -> usize {
    crate::keys::BINDINGS
        .iter()
        .map(|b| b.hint.len())
        .max()
        .unwrap_or(0)
}

/// Write the first-run config to `path` atomically
/// ([`crate::paths::write_atomic`]). The parent is made with
/// [`crate::paths::create_private_parent`], not the `/tmp`-hardened path,
/// since an existing `~/.config` must be accepted.
pub fn write_default(path: &Path, prefix: u8) -> Result<(), ConfigError> {
    let err = |error| ConfigError::Write {
        path: path.to_path_buf(),
        error,
    };
    crate::paths::create_private_parent(path).map_err(err)?;
    crate::paths::write_atomic(path, render_default_config(prefix).as_bytes()).map_err(err)
}

// --- saving ---------------------------------------------------------------

/// The file a save writes: the one [`load`] reads, so `$NVMUX_CONFIG` if set
/// (the first run never writes there, but a save is user-requested).
fn config_path() -> Option<PathBuf> {
    let nvmux_config = std::env::var_os("NVMUX_CONFIG");
    let xdg = std::env::var_os("XDG_CONFIG_HOME");
    let home = std::env::var_os("HOME");
    match resolve_config_path(nvmux_config.as_deref(), xdg.as_deref(), home.as_deref()) {
        ConfigSource::Explicit(path) | ConfigSource::Default(path) => Some(path),
        ConfigSource::None => None,
    }
}

/// `path` as the user would type it, with `~/` for home.
pub(crate) fn shown(path: &Path) -> String {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty());
    match home.and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf)) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// What a save wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    pub path: PathBuf,
    /// Settings changed; 0 if the file already matched.
    pub count: usize,
}

impl std::fmt::Display for Saved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.count {
            0 => write!(f, "nothing to write: {} already says so", shown(&self.path)),
            1 => write!(f, "saved 1 change to {}", shown(&self.path)),
            n => write!(f, "saved {n} changes to {}", shown(&self.path)),
        }
    }
}

/// Write what `live` changed from `saved` (the file's last known contents)
/// into the config file, and nothing else.
///
/// The file is re-read first, as it may have been hand-edited: one that no
/// longer parses is not overwritten, and a setting also changed in the file
/// is not overwritten unless `overwrite`. Each setting is written where the
/// file has it, comments kept: in place if set, by uncommenting the
/// template's line if not, else at the end of its table. One reset to its
/// default is commented back out. With no file, the first-run template is
/// written with the changes. Symlinks are followed and the target's mode kept
/// (see [`crate::paths::write_through`]).
pub fn save(live: &Settings, saved: &Settings, overwrite: bool) -> Result<Saved, SaveError> {
    let path = config_path().ok_or(SaveError::Nowhere)?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(SaveError::Read { path, error }),
    };
    let (contents, count) = match saved_text(text.as_deref(), live, saved, overwrite) {
        Ok(edited) => edited,
        Err(Refusal::Unreadable(message)) => return Err(SaveError::Unreadable { path, message }),
        Err(Refusal::Changed(settings)) => return Err(SaveError::Changed { path, settings }),
        Err(Refusal::Unedited) => return Err(SaveError::Unedited { path }),
    };
    if count > 0 {
        let write = |error| SaveError::Write {
            path: path.clone(),
            error,
        };
        if text.is_none() {
            crate::paths::create_private_parent(&path).map_err(write)?;
        }
        crate::paths::write_through(&path, contents.as_bytes()).map_err(write)?;
    }
    Ok(Saved { path, count })
}

/// Why [`saved_text`] edited nothing.
#[derive(Debug, PartialEq, Eq)]
enum Refusal {
    /// The text no longer parses: where and why, in one line.
    Unreadable(String),
    /// Paths of settings being saved that the text also changed.
    Changed(Vec<String>),
    /// No edit produced the intended settings.
    Unedited,
}

/// `text` (`None` if there is no file) with `live`'s changes from `saved`
/// written in, and how many settings changed. See [`save`].
fn saved_text(
    text: Option<&str>,
    live: &Settings,
    saved: &Settings,
    overwrite: bool,
) -> Result<(String, usize), Refusal> {
    let on_disk = match text {
        Some(text) => read_back(text).map_err(Refusal::Unreadable)?,
        None => Settings::default(),
    };
    let changed = crate::settings::changed(live, saved);
    // Also changed in the file, to something else: a save would silently undo
    // that edit.
    let clashing: Vec<String> = changed
        .iter()
        .filter(|s| {
            let disk = s.value(&on_disk);
            disk != s.value(saved) && disk != s.value(live)
        })
        .map(|s| s.path())
        .collect();
    if !clashing.is_empty() && !overwrite {
        return Err(Refusal::Changed(clashing));
    }

    // The file's current settings plus each change.
    let mut target = on_disk.clone();
    let mut edits = Vec::new();
    for setting in changed {
        let value = setting.value(live);
        if setting.value(&on_disk) == value {
            continue;
        }
        setting.set(&mut target, &value);
        edits.push(Edit {
            table: setting.table,
            key: setting.key,
            value: value.toml().to_string(),
            default: value == setting.default_value(),
        });
    }
    let base = match text {
        Some(text) => text.to_string(),
        None if edits.is_empty() => String::new(),
        None => render_default_config(crate::keys::PREFIX),
    };
    if edits.is_empty() {
        return Ok((base, 0));
    }
    let holds = |text: &String| read_back(text).is_ok_and(|s| s == target);
    edit_in_place(&base, &edits)
        .filter(holds)
        .or_else(|| edit_structure(&base, &edits).filter(holds))
        .map(|text| (text, edits.len()))
        .ok_or(Refusal::Unedited)
}

/// `text` as nvmux would load it, or why not, in a line.
fn read_back(text: &str) -> Result<Settings, String> {
    let settings: Settings = toml::from_str(text).map_err(|e| {
        let at = e
            .span()
            .map(|span| text[..span.start.min(text.len())].matches('\n').count() + 1);
        match at {
            Some(line) => format!("line {line}: {}", e.message()),
            None => e.message().to_string(),
        }
    })?;
    settings.validate()?;
    Ok(settings)
}

/// One setting to write: where, what, and whether that is its default.
#[derive(Debug)]
struct Edit {
    table: &'static str,
    key: &'static str,
    /// As the file spells it, `true` or `"Ctrl-a"`.
    value: String,
    default: bool,
}

/// Apply `edits` in place, keeping every other byte.
///
/// A set setting has its value replaced, its note kept in its column, or its
/// line commented out if back at its default. An unset one is uncommented
/// where its table has it commented (as the template has every setting),
/// else added after its table's last setting, or in a new table at the end.
/// `None` for layouts this cannot edit (tables only inline or in dotted
/// keys), which [`edit_structure`] handles.
fn edit_in_place(text: &str, edits: &[Edit]) -> Option<String> {
    let doc = toml_edit::Document::parse(text).ok()?;
    let headers = headers(doc.as_table());
    // (where, replacing how much, with what), all against the original text.
    let mut changes: Vec<(usize, usize, String)> = Vec::new();
    let mut after: Vec<(usize, Vec<String>)> = Vec::new();
    let mut tables: Vec<(&str, Vec<String>)> = Vec::new();
    for edit in edits {
        let line = format!("{} = {}", edit.key, edit.value);
        let path: Vec<&str> = edit.table.split('.').collect();
        if let Some((span, inline)) = active(doc.as_item(), &path, edit.key) {
            let (start, end) = line_around(text, span.start, span.end);
            let rest = &text[span.end..end];
            let old = &text[span.clone()];
            if edit.default && !inline {
                // Commented back out, at its default, as the template has it.
                let head = &text[start..span.start];
                let indent = &head[..head.len() - head.trim_start().len()];
                let kept = realigned(rest, old, &edit.value).unwrap_or_else(|| rest.to_string());
                changes.push((
                    start,
                    end - start,
                    format!("{indent}# {}{}{kept}", head.trim_start(), edit.value),
                ));
            } else {
                match realigned(rest, old, &edit.value) {
                    Some(kept) => changes.push((
                        span.start,
                        end - span.start,
                        format!("{}{kept}", edit.value),
                    )),
                    None => changes.push((span.start, span.len(), edit.value.clone())),
                }
            }
            continue;
        }
        if edit.default {
            // Nothing pins it: the file already follows the default.
            continue;
        }
        match region(text, &headers, edit.table) {
            Some(region) => match commented(text, region.body.clone(), edit.key) {
                Some((start, end)) => {
                    changes.push((
                        start,
                        end - start,
                        uncommented(&text[start..end], &edit.value),
                    ));
                }
                None => {
                    let at = last_setting_end(text, region);
                    match after.iter_mut().find(|(here, _)| *here == at) {
                        Some((_, lines)) => lines.push(line),
                        None => after.push((at, vec![line])),
                    }
                }
            },
            // The table exists without its own header; do not add one.
            None if defined(doc.as_item(), &path) => return None,
            None => match tables.iter_mut().find(|(table, _)| *table == edit.table) {
                Some((_, lines)) => lines.push(line),
                None => tables.push((edit.table, vec![line])),
            },
        }
    }
    for (at, lines) in after {
        changes.push((at, 0, lines.iter().map(|l| format!("\n{l}")).collect()));
    }
    changes.sort_by_key(|(at, _, _)| std::cmp::Reverse(*at));
    let mut out = text.to_string();
    let mut limit = out.len();
    for (at, len, with) in changes {
        // Edits never meet: each is a line, or a point after one.
        if at + len > limit {
            return None;
        }
        out.replace_range(at..at + len, &with);
        limit = at;
    }
    for (table, lines) in tables {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        if !out.is_empty() && !out.ends_with("\n\n") {
            out.push('\n');
        }
        out.push_str(&format!("[{table}]\n"));
        for line in lines {
            out.push_str(&line);
            out.push('\n');
        }
    }
    Some(out)
}

/// The span of `key`'s value in the table at `path`, and whether it is in an
/// inline table (where it cannot be commented out); `None` if unset.
fn active(
    root: &toml_edit::Item,
    path: &[&str],
    key: &str,
) -> Option<(std::ops::Range<usize>, bool)> {
    let mut at = root;
    let mut inline = false;
    for part in path {
        inline |= at.is_inline_table();
        at = at.as_table_like()?.get(part)?;
    }
    inline |= at.is_inline_table();
    let value = at.as_table_like()?.get(key)?.as_value()?;
    Some((value.span()?, inline))
}

/// Whether the table at `path` is defined other than by its own header
/// (inline or dotted keys). An implicit table (`[effects]` existing only via
/// `[effects.fade]`) is not, since TOML allows adding its header later.
fn defined(root: &toml_edit::Item, path: &[&str]) -> bool {
    let mut at = root;
    for part in path {
        match at.as_table_like().and_then(|t| t.get(part)) {
            Some(item) => at = item,
            None => return false,
        }
    }
    !at.as_table().is_some_and(toml_edit::Table::is_implicit)
}

/// Every explicit table header (`[effects.fade]`) by path, with its start.
fn headers(root: &toml_edit::Table) -> Vec<(String, usize)> {
    fn walk(table: &toml_edit::Table, at: &str, out: &mut Vec<(String, usize)>) {
        for (key, item) in table.iter() {
            let path = if at.is_empty() {
                key.to_string()
            } else {
                format!("{at}.{key}")
            };
            let tables: Vec<&toml_edit::Table> = match item {
                toml_edit::Item::Table(t) => vec![t],
                toml_edit::Item::ArrayOfTables(a) => a.iter().collect(),
                _ => continue,
            };
            for t in tables {
                if !t.is_implicit() && !t.is_dotted() {
                    if let Some(span) = t.span() {
                        out.push((path.clone(), span.start));
                    }
                }
                walk(t, &path, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, "", &mut out);
    out
}

/// Where `table`'s header line ends, and its body up to the next header;
/// `None` if it has no header.
fn region(text: &str, headers: &[(String, usize)], table: &str) -> Option<Region> {
    let start = headers.iter().find(|(path, _)| path == table)?.1;
    let header_end = line_around(text, start, start).1;
    let body = (header_end + 1).min(text.len());
    let end = headers
        .iter()
        .map(|(_, at)| *at)
        .filter(|at| *at > start)
        .min()
        .map_or(text.len(), |at| line_around(text, at, at).0);
    Some(Region {
        header_end,
        body: body..end.max(body),
    })
}

/// See [`region`].
#[derive(Debug, Clone)]
struct Region {
    /// The end of the header's line, short of its newline.
    header_end: usize,
    body: std::ops::Range<usize>,
}

/// The start of the line `from` is on, and the end of the line `to` is on,
/// short of its newline.
fn line_around(text: &str, from: usize, to: usize) -> (usize, usize) {
    let start = text[..from].rfind('\n').map_or(0, |at| at + 1);
    let end = text[to..].find('\n').map_or(text.len(), |at| to + at);
    (start, end)
}

/// A line in `region` that is `key` commented out, `# key = …`, as the
/// template has every setting: where it starts and ends.
fn commented(text: &str, region: std::ops::Range<usize>, key: &str) -> Option<(usize, usize)> {
    let mut at = region.start;
    for line in text[region.clone()].split_inclusive('\n') {
        let start = at;
        at += line.len();
        let rest = line.trim_start().strip_prefix('#').map(str::trim_start);
        let is_key = rest
            .and_then(|r| r.strip_prefix(key))
            .is_some_and(|r| r.trim_start().starts_with('='));
        if is_key {
            return Some((start, start + line.trim_end_matches(['\n', '\r']).len()));
        }
    }
    None
}

/// `# key = old   # note` as `key = new   # note`: the key, its `=` and
/// the note as they were written, the note in its column relative to them.
fn uncommented(line: &str, value: &str) -> String {
    let indent = &line[..line.len() - line.trim_start().len()];
    let rest = line.trim_start().trim_start_matches('#').trim_start();
    // `commented` matched `key =`, so there is an `=`.
    let eq = rest.find('=').unwrap_or(rest.len().saturating_sub(1));
    let after = &rest[eq + 1..];
    let value_at = after.len() - after.trim_start().len();
    let old = old_value(&after[value_at..]);
    let tail = &after[value_at + old.len()..];
    let kept = realigned(tail, old, value).unwrap_or_default();
    format!(
        "{indent}{}{}{value}{kept}",
        &rest[..=eq],
        &after[..value_at]
    )
}

/// The value at the start of `text`: a quoted string to its closing quote, or
/// anything else to the first blank or `#`.
fn old_value(text: &str) -> &str {
    let mut chars = text.char_indices();
    match chars.next() {
        Some((_, quote @ ('"' | '\''))) => {
            let mut escaped = false;
            for (i, c) in chars {
                match c {
                    '\\' if quote == '"' && !escaped => escaped = true,
                    c if c == quote && !escaped => return &text[..i + 1],
                    _ => escaped = false,
                }
            }
            text
        }
        _ => {
            let end = text
                .find(|c: char| c.is_whitespace() || c == '#')
                .unwrap_or(text.len());
            &text[..end]
        }
    }
}

/// If what follows a value is a note (blanks then `#`), re-pad it so the
/// note keeps its column after the new value; `None` otherwise.
fn realigned(rest: &str, old: &str, new: &str) -> Option<String> {
    let note = rest.trim_start_matches([' ', '\t']);
    if !note.starts_with('#') {
        return None;
    }
    let gap = rest.len() - note.len();
    let width = (gap + old.chars().count())
        .saturating_sub(new.chars().count())
        .max(1);
    Some(format!("{}{note}", " ".repeat(width)))
}

/// Where a line added to a table goes: after its last setting, or under its
/// header if it has none, before any comments leading into the next table.
fn last_setting_end(text: &str, region: Region) -> usize {
    let mut end = region.header_end;
    let mut at = region.body.start;
    for line in text[region.body].split_inclusive('\n') {
        let trimmed = line.trim();
        if !trimmed.is_empty() && !trimmed.starts_with('#') {
            end = at + line.trim_end_matches(['\n', '\r']).len();
        }
        at += line.len();
    }
    end
}

/// Edits through `toml_edit`'s document model, for tables inline or in
/// dotted keys, which [`edit_in_place`] skips. Comments survive, but a value
/// reset to its default is removed, not commented out.
fn edit_structure(text: &str, edits: &[Edit]) -> Option<String> {
    let mut doc: toml_edit::DocumentMut = text.parse().ok()?;
    for edit in edits {
        let mut item = doc.as_item_mut();
        for part in edit.table.split('.') {
            item = item
                .as_table_like_mut()?
                .entry(part)
                .or_insert(toml_edit::table());
        }
        // An implicit table gets a header now it has a setting of its own.
        if let Some(table) = item.as_table_mut() {
            table.set_implicit(false);
        }
        let table = item.as_table_like_mut()?;
        if edit.default {
            table.remove(edit.key);
        } else {
            let value: toml_edit::Value = edit.value.parse().ok()?;
            match table
                .get_mut(edit.key)
                .and_then(toml_edit::Item::as_value_mut)
            {
                // Its decor kept: the note after it stays.
                Some(old) => {
                    let decor = old.decor().clone();
                    *old = value;
                    *old.decor_mut() = decor;
                }
                None => {
                    table.insert(edit.key, toml_edit::Item::Value(value));
                }
            }
        }
    }
    Some(doc.to_string())
}

/// The live settings and what the file is known to hold. [`get`] uses a
/// process-global one; tests make their own so changes do not leak between
/// them.
struct Store {
    /// What [`get`] returns: as loaded, plus any changes since.
    live: RwLock<Arc<Settings>>,
    /// What the file holds as far as this process knows (as loaded, or as
    /// last saved). Differences from `live` are unsaved.
    saved: RwLock<Arc<Settings>>,
    /// Changes `live` has seen; see [`generation`].
    generation: AtomicU64,
    initialised: AtomicBool,
}

impl Store {
    fn new() -> Self {
        let defaults = Arc::new(Settings::default());
        Self {
            live: RwLock::new(defaults.clone()),
            saved: RwLock::new(defaults),
            generation: AtomicU64::new(0),
            initialised: AtomicBool::new(false),
        }
    }

    fn init(&self, settings: Settings) {
        if self.initialised.swap(true, Ordering::SeqCst) {
            tracing::warn!("settings initialised more than once; keeping the first");
            return;
        }
        let settings = Arc::new(settings);
        *write(&self.saved) = settings.clone();
        *write(&self.live) = settings;
    }

    fn get(&self) -> Arc<Settings> {
        read(&self.live).clone()
    }

    fn apply(&self, settings: Settings) -> Result<(), String> {
        settings.validate()?;
        let mut live = write(&self.live);
        if **live != settings {
            *live = Arc::new(settings);
            self.generation.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }

    fn saved(&self) -> Arc<Settings> {
        read(&self.saved).clone()
    }

    fn set_saved(&self, settings: Settings) {
        *write(&self.saved) = Arc::new(settings);
    }
}

/// Ignores poisoning: values are only ever swapped in whole, so a poisoned
/// lock still holds valid settings.
fn read(lock: &RwLock<Arc<Settings>>) -> std::sync::RwLockReadGuard<'_, Arc<Settings>> {
    lock.read().unwrap_or_else(|e| e.into_inner())
}

fn write(lock: &RwLock<Arc<Settings>>) -> std::sync::RwLockWriteGuard<'_, Arc<Settings>> {
    lock.write().unwrap_or_else(|e| e.into_inner())
}

static STORE: LazyLock<Store> = LazyLock::new(Store::new);

/// Install the loaded settings once, before anything reads them (`main`, and
/// the relay tests' child process). A second call warns and keeps the first.
/// They also become [`saved`].
pub fn init(settings: Settings) {
    STORE.init(settings);
}

/// The current settings: as loaded, plus settings-screen changes. Defaults if
/// [`init`] has not run, so unit tests never read a real file. Call it each
/// time a value is needed; the `Arc` is a snapshot.
pub fn get() -> Arc<Settings> {
    STORE.get()
}

/// Switch to `settings` if they pass [`load`]'s validation, bumping
/// [`generation`]. Applying the current settings is a no-op.
pub fn apply(settings: Settings) -> Result<(), String> {
    STORE.apply(settings)
}

/// Number of changes [`apply`] has made. A reader holding a copy (a client's
/// settings, the picker's effects) compares it to detect a stale copy.
pub fn generation() -> u64 {
    STORE.generation.load(Ordering::SeqCst)
}

/// What the file holds as far as nvmux knows: what [`init`] got, until
/// [`set_saved`]. Where [`get`] differs, it is unsaved.
pub fn saved() -> Arc<Settings> {
    STORE.saved()
}

/// Record that a save wrote `settings`.
pub fn set_saved(settings: Settings) {
    STORE.set_saved(settings);
}

static REMOTE: AtomicBool = AtomicBool::new(false);

/// Record whether the sessions are on another machine, over ssh. Called once
/// by `main` before starting a client. Clients receive it with their settings
/// (`"auto"` predicts only when remote, see [`Drawing`]), and the settings
/// screen checks `[client] ui` changes against it.
pub fn set_remote(remote: bool) {
    REMOTE.store(remote, Ordering::Relaxed);
}

/// As [`set_remote`] said; `false` until then (unit tests, and clients
/// themselves).
pub fn remote() -> bool {
    REMOTE.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // --- defaults / parsing (pure) -----------------------------------------

    /// `keys::PREFIX` is the one default defined elsewhere (`--help` and the
    /// first-run screen print it before any config loads). The others are
    /// pinned by `a_full_document_at_the_defaults_round_trips`.
    #[test]
    fn the_default_prefix_is_the_one_the_key_machine_uses() {
        assert_eq!(KeySettings::default().prefix, crate::keys::PREFIX);
    }

    #[test]
    fn an_empty_document_is_the_defaults() {
        let s: Settings = toml::from_str("").expect("empty is valid");
        assert_eq!(s, Settings::default());
    }

    /// The TOML of the README's "Every setting, at its default".
    fn readme_settings() -> &'static str {
        let readme = include_str!("../README.md");
        let start = readme
            .find("<summary>Every setting, at its default</summary>")
            .expect("the README's settings block");
        let block = &readme[start..];
        let open = block.find("```toml\n").expect("its TOML") + "```toml\n".len();
        let close = open + block[open..].find("```").expect("its end");
        &block[open..close]
    }

    /// That README block is a hand-kept copy of the defaults: it must parse to
    /// them and include every table.
    #[test]
    fn the_readmes_settings_are_the_defaults() {
        let doc = readme_settings();
        let s: Settings = toml::from_str(doc).expect("the README's settings parse");
        assert_eq!(
            s,
            Settings::default(),
            "the README's settings are not the defaults"
        );
        for table in [
            "[keys]",
            "[keys.bind]",
            "[session]",
            "[client]",
            "[style]",
            "[effects]",
            "[effects.smear]",
            "[effects.particles]",
            "[effects.scroll]",
            "[effects.windows]",
            "[effects.blink]",
        ] {
            assert!(doc.contains(table), "the README has no {table}");
        }
    }

    #[test]
    fn a_full_document_at_the_defaults_round_trips() {
        let doc = "\
            [keys]\n\
            prefix = \"Ctrl-Space\"\n\
            timeout_ms = 1000\n\
            [keys.bind]\n\
            picker = \"Space\"\n\
            detach = \"d\"\n\
            new = \"c\"\n\
            next = \"n\"\n\
            prev = \"p\"\n\
            go = \"Enter\"\n\
            help = \"?\"\n\
            [session]\n\
            command = \"nvim --headless --listen {sock}\"\n\
            [client]\n\
            ui = \"nvmux\"\n\
            per_session = true\n\
            lazy = false\n\
            predict_scrolls = \"auto\"\n\
            predict_motions = \"auto\"\n\
            predict_typing = \"auto\"\n\
            centre = \"off\"\n\
            [style]\n\
            color = \"terminal\"\n\
            float = true\n\
            [effects]\n\
            enabled = true\n\
            [effects.fade]\n\
            enabled = true\n\
            duration_ms = 200\n\
            session = true\n\
            [effects.move]\n\
            enabled = true\n\
            [effects.cursor]\n\
            enabled = true\n\
            [effects.attach]\n\
            enabled = true\n\
            [effects.kill]\n\
            enabled = true\n\
            [effects.filter]\n\
            enabled = true\n\
            [effects.create]\n\
            enabled = true\n\
            [effects.smear]\n\
            enabled = false\n\
            [effects.particles]\n\
            enabled = false\n\
            mode = \"railgun\"\n\
            [effects.scroll]\n\
            enabled = true\n\
            [effects.windows]\n\
            enabled = true\n\
            [effects.blink]\n\
            enabled = false\n";
        let s: Settings = toml::from_str(doc).expect("valid");
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn a_partial_fade_table_keeps_the_other_fade_defaults() {
        let s: Settings = toml::from_str("[effects.fade]\nduration_ms = 40\n").expect("valid");
        assert_eq!(s.effects.fade.duration_ms, 40);
        assert_eq!(s.effects.fade.enabled, FadeSettings::default().enabled);
        assert_eq!(s.effects.fade.session, FadeSettings::default().session);
        assert!(s.effects.enabled, "the master switch keeps its default");
        assert_eq!(s.effects.moving, MoveSettings::default());
        assert_eq!(s.keys, KeySettings::default());
    }

    /// `duration_ms = 1` is legal, and halving it in whole milliseconds would
    /// give a zero-length schedule, which `validate` exists to forbid.
    #[test]
    fn one_direction_is_half_the_configured_dissolve() {
        let at = |ms| {
            FadeSettings {
                duration_ms: ms,
                ..FadeSettings::default()
            }
            .one_way()
        };

        assert_eq!(at(200), Duration::from_millis(100), "the default");
        assert_eq!(at(MAX_FADE_MS), Duration::from_millis(1_000), "the ceiling");
        assert_eq!(
            at(101),
            Duration::from_micros(50_500),
            "an odd value is not rounded away"
        );
        assert_eq!(at(1), Duration::from_micros(500), "the floor is not zero");
        assert!(!at(1).is_zero(), "a legal setting must have some length");
    }

    #[test]
    fn fade_enabled_false_parses() {
        let s: Settings = toml::from_str("[effects.fade]\nenabled = false\n").expect("valid");
        assert!(!s.effects.fade.enabled);
        assert!(!s.effects.fade_enabled());
        assert!(s.effects.move_enabled(), "the other effect is untouched");
    }

    /// Each effect's own switch is kept for when the master is turned back on.
    #[test]
    fn the_master_switch_turns_every_effect_off() {
        let s: Settings = toml::from_str("[effects]\nenabled = false\n").expect("valid");
        assert!(!s.effects.fade_enabled());
        assert!(!s.effects.move_enabled());
        assert!(!s.effects.cursor_enabled());
        assert!(!s.effects.attach_enabled());
        assert!(!s.effects.kill_enabled());
        assert!(!s.effects.filter_enabled());
        assert!(!s.effects.create_enabled());
        assert!(!s.effects.smear_enabled());
        assert!(!s.effects.scroll_enabled());
        assert!(!s.effects.windows_enabled());
        assert!(!s.effects.particles_enabled() && !s.effects.blink_enabled());
        assert!(s.effects.scroll.enabled && s.effects.windows.enabled);
        assert!(s.effects.fade.enabled && s.effects.moving.enabled);
        assert!(s.effects.cursor.enabled && s.effects.attach.enabled);
        assert!(s.effects.kill.enabled && s.effects.filter.enabled);
        assert!(s.effects.create.enabled);
        let on = Settings::default().effects;
        assert!(on.fade_enabled() && on.move_enabled() && on.cursor_enabled());
        assert!(on.attach_enabled());
        assert!(on.kill_enabled() && on.filter_enabled());
        assert!(on.create_enabled());
    }

    #[test]
    fn each_picker_effect_turns_off_on_its_own() {
        let s: Settings = toml::from_str("[effects.cursor]\nenabled = false\n").expect("valid");
        assert!(!s.effects.cursor_enabled());
        assert!(s.effects.attach_enabled() && s.effects.kill_enabled());
        assert!(s.effects.filter_enabled());

        let s: Settings = toml::from_str("[effects.attach]\nenabled = false\n").expect("valid");
        assert!(!s.effects.attach_enabled());
        assert!(s.effects.cursor_enabled() && s.effects.kill_enabled());
        assert!(s.effects.fade_enabled(), "the fade itself is untouched");

        let s: Settings = toml::from_str("[effects.kill]\nenabled = false\n").expect("valid");
        assert!(!s.effects.kill_enabled());
        assert!(s.effects.cursor_enabled() && s.effects.attach_enabled());
        assert!(s.effects.filter_enabled());

        let s: Settings = toml::from_str("[effects.filter]\nenabled = false\n").expect("valid");
        assert!(!s.effects.filter_enabled());
        assert!(s.effects.cursor_enabled() && s.effects.attach_enabled());
        assert!(s.effects.kill_enabled() && s.effects.create_enabled());

        let s: Settings = toml::from_str("[effects.create]\nenabled = false\n").expect("valid");
        assert!(!s.effects.create_enabled());
        assert!(s.effects.cursor_enabled() && s.effects.filter_enabled());
        assert!(s.effects.fade_enabled(), "the fade itself is untouched");
    }

    #[test]
    fn the_move_effect_is_on_unless_turned_off() {
        assert!(Settings::default().effects.moving.enabled);
        let s: Settings = toml::from_str("[effects.move]\nenabled = false\n").expect("valid");
        assert!(!s.effects.move_enabled());
        assert!(s.effects.fade_enabled(), "and the fade is untouched");
    }

    #[test]
    fn one_client_per_session_is_on_unless_turned_off() {
        assert!(Settings::default().client.per_session);
        let s: Settings = toml::from_str("[client]\nper_session = false\n").expect("valid");
        assert!(!s.client.per_session);
        assert_eq!(s.effects, EffectsSettings::default());
        assert_eq!(s.session, SessionSettings::default());
    }

    /// `lazy` and `per_session` are independent.
    #[test]
    fn clients_start_at_launch_unless_made_lazy() {
        assert!(!Settings::default().client.lazy);
        let s: Settings = toml::from_str("[client]\nlazy = true\n").expect("valid");
        assert!(s.client.lazy);
        assert!(s.client.per_session, "and the clients are still kept");
        let s: Settings =
            toml::from_str("[client]\nper_session = false\nlazy = true\n").expect("valid");
        assert!(s.client.lazy && !s.client.per_session);
    }

    /// Each `predict_*` is independent. Values survive the client's
    /// environment; old booleans and other words are refused, naming the
    /// valid ones.
    #[test]
    fn auto_predicts_over_ssh_and_not_here_unless_told() {
        let (auto, t, f) = (Predict::Auto, true, false);
        let c = Settings::default().client;
        assert_eq!(
            (c.predict_scrolls, c.predict_motions, c.predict_typing),
            (auto, auto, auto)
        );
        let there = c.predictions(true);
        assert_eq!((there.scrolls, there.motions, there.typing), (t, t, t));
        assert!(!c.predictions(false).any(), "a session on this machine");
        assert!(c.predicts(true) && !c.predicts(false));
        // (scrolls, motions, typing) over ssh and locally.
        for (doc, there, here) in [
            ("predict_typing = \"off\"", (t, t, f), (f, f, f)),
            ("predict_motions = \"on\"", (t, t, t), (f, t, f)),
            (
                "predict_scrolls = \"on\"\npredict_motions = \"off\"",
                (t, f, t),
                (t, f, f),
            ),
            (
                "predict_scrolls = \"off\"\npredict_typing = \"on\"",
                (f, t, t),
                (f, f, t),
            ),
            (
                "predict_scrolls = \"on\"\npredict_motions = \"on\"\npredict_typing = \"on\"",
                (t, t, t),
                (t, t, t),
            ),
            (
                "predict_scrolls = \"off\"\npredict_motions = \"off\"\npredict_typing = \"off\"",
                (f, f, f),
                (f, f, f),
            ),
        ] {
            let doc = format!("[client]\n{doc}\n");
            let s: Settings = toml::from_str(&doc).expect(&doc);
            for (remote, (scrolls, motions, typing)) in [(true, there), (false, here)] {
                let p = s.client.predictions(remote);
                assert_eq!(
                    (p.scrolls, p.motions, p.typing),
                    (scrolls, motions, typing),
                    "{doc} over ssh: {remote}"
                );
                assert_eq!(p.acks(), motions || typing, "{doc}");
                assert_eq!(s.client.predicts(remote), p.any(), "{doc}");
            }
            assert!(s.client.per_session && !s.client.lazy, "the rest as it was");
            let drawing = Drawing {
                remote: true,
                ..s.drawing()
            };
            assert_eq!(Drawing::from_env(&drawing.to_env()), Some(drawing));
        }
        for word in ["true", "false", "\"scrolls\"", "\"yes\""] {
            let doc = format!("[client]\npredict_motions = {word}\n");
            let msg = toml::from_str::<Settings>(&doc)
                .expect_err(&doc)
                .to_string();
            assert!(msg.contains(r#"expected "auto", "on" or "off""#), "{msg}");
        }
    }

    /// `"auto"` and `"on"` load as if unset; `"off"` is refused naming the
    /// replacements, since ignoring it would re-enable predictions. Old
    /// booleans are refused too, and clients are not sent it.
    #[test]
    fn a_file_that_still_sets_predict_says_what_it_says_now() {
        for was in ["auto", "on"] {
            let doc = format!("[client]\npredict = \"{was}\"\nlazy = true\n");
            let s: Settings = toml::from_str(&doc).expect(&doc);
            assert_eq!(s, toml::from_str("[client]\nlazy = true\n").expect("valid"));
        }
        let msg = toml::from_str::<Settings>("[client]\npredict = \"off\"\n")
            .expect_err("off")
            .to_string();
        assert!(
            msg.contains("predict_scrolls, predict_motions and predict_typing")
                && msg.contains("for \"off\", set all three to \"off\""),
            "{msg}"
        );
        for wrong in ["\"yes\"", "true", "false"] {
            let doc = format!("[client]\npredict = {wrong}\n");
            let msg = toml::from_str::<Settings>(&doc)
                .expect_err(&doc)
                .to_string();
            assert!(
                msg.contains("predict_scrolls, predict_motions and predict_typing"),
                "{msg}"
            );
        }
        let env = Settings::default().drawing().to_env();
        assert!(!env.contains("\"predict\""), "{env}");
    }

    /// The old boolean `centre` and `smart_width` are errors.
    #[test]
    fn nothing_is_centred_unless_told() {
        let c = Settings::default().client;
        assert_eq!((c.centre, c.window_width), (Centre::Off, 120));
        for way in Centre::ALL {
            let doc = format!("[client]\ncentre = \"{}\"\n", way.name());
            let s: Settings = toml::from_str(&doc).expect("valid");
            assert_eq!(s.client.centre, way);
            assert_eq!(s.client.window_width, 120, "the rest as it was");
        }
        let s: Settings = toml::from_str("[client]\nwindow_width = 0\n").expect("valid");
        assert_eq!(s.client.window_width, 0);
        for doc in [
            "[client]\ncentre = true\n",
            "[client]\ncentre = \"windows\"\n",
        ] {
            let msg = toml::from_str::<Settings>(doc).expect_err(doc).to_string();
            assert!(
                msg.contains(r#"expected "off", "smart" or "fixed""#),
                "{msg}"
            );
        }
        for doc in [
            "[client]\nsmart_width = true\n",
            "[client]\nwindow_width = -1\n",
        ] {
            assert!(toml::from_str::<Settings>(doc).is_err(), "{doc:?}");
        }
    }

    /// Any boolean loads and is ignored; a non-boolean is still an error.
    /// Clients are not sent them.
    #[test]
    fn a_file_that_still_sets_a_retired_switch_loads_as_one_that_does_not() {
        let retired = [
            ("session", "quick_list"),
            ("session", "push_news"),
            ("client", "revive"),
            ("client", "throttle"),
            ("client", "lean_output"),
            ("client", "scroll_regions"),
            ("client", "partial_frames"),
        ];
        for (table, key) in retired {
            for value in ["true", "false"] {
                let with = format!("[{table}]\n{key} = {value}\n");
                let s: Settings = toml::from_str(&with).expect("valid");
                assert_eq!(s, Settings::default(), "{with:?}");
            }
            let wrong = format!("[{table}]\n{key} = 1\n");
            assert!(toml::from_str::<Settings>(&wrong).is_err(), "{wrong:?}");
        }
        let s: Settings = toml::from_str("[client]\nrevive = true\nlazy = true\n").expect("valid");
        assert_eq!(s, toml::from_str("[client]\nlazy = true\n").expect("valid"));
        let env = Settings::default().drawing().to_env();
        for (_, key) in retired {
            assert!(!env.contains(key), "{env}");
        }
    }

    #[test]
    fn a_session_is_drawn_by_nvmuxs_client_unless_told() {
        assert_eq!(Settings::default().client.ui, Ui::Nvmux);
        let s: Settings = toml::from_str("[client]\nui = \"nvim\"\n").expect("valid");
        assert_eq!(s.client.ui, Ui::Nvim);
        assert!(s.client.per_session && !s.client.lazy, "the rest as it was");
    }

    /// Both `#rgb` and `#rrggbb` parse; `name` gives the long form.
    #[test]
    fn the_screens_are_in_the_terminals_colours_unless_told() {
        assert_eq!(Settings::default().style.color, ThemeColor::Terminal);
        assert_eq!(ThemeColor::Terminal.name(), "terminal");
        for (spelt, rgb, name) in [
            ("#7aa2f7", Rgb(0x7a, 0xa2, 0xf7), "#7aa2f7"),
            ("#7AA2F7", Rgb(0x7a, 0xa2, 0xf7), "#7aa2f7"),
            ("#f80", Rgb(0xff, 0x88, 0x00), "#ff8800"),
            ("#000", Rgb(0, 0, 0), "#000000"),
        ] {
            let s: Settings =
                toml::from_str(&format!("[style]\ncolor = \"{spelt}\"\n")).expect(spelt);
            assert_eq!(s.style.color, ThemeColor::Mono(rgb), "{spelt}");
            assert_eq!(s.style.color.name(), name, "{spelt}");
            assert_eq!(
                ThemeColor::parse(&s.style.color.name()),
                Ok(s.style.color),
                "{spelt} round trips"
            );
            assert_eq!(s.effects, EffectsSettings::default(), "the rest as it was");
        }
        let s: Settings = toml::from_str("[style]\ncolor = \"terminal\"\n").expect("valid");
        assert_eq!(s, Settings::default(), "the default, said out loud");
    }

    #[test]
    fn the_screens_float_over_the_session_unless_told() {
        assert!(Settings::default().style.float);
        let s: Settings = toml::from_str("[style]\nfloat = false\n").expect("valid");
        assert!(!s.style.float);
        assert_eq!(s.style.color, ThemeColor::Terminal, "the colour as it was");
        let s: Settings = toml::from_str("[style]\nfloat = true\n").expect("valid");
        assert_eq!(s, Settings::default(), "the default, said out loud");
        assert!(
            toml::from_str::<Settings>("[style]\nfloat = \"yes\"\n").is_err(),
            "a switch is true or false"
        );
    }

    #[test]
    fn a_colour_that_is_not_one_says_what_would_be() {
        for bad in [
            "",
            "blue",
            "Terminal",
            "7aa2f7",
            "#7aa2f",
            "#7aa2f7f",
            "#7aa2fg",
            " #7aa2f7",
            "#7aa2f7 ",
            "#ffff88880000",
            "rgb:7a/a2/f7",
        ] {
            let err = ThemeColor::parse(bad).expect_err(bad);
            assert!(
                err.starts_with("color ") && err.contains("\"#7aa2f7\""),
                "{bad:?} -> {err:?}"
            );
        }
    }

    #[test]
    fn the_clients_effects_start_as_neovides() {
        let e = Settings::default().effects;
        assert!(e.scroll_enabled() && e.windows_enabled());
        assert!(!e.smear_enabled() && !e.particles_enabled() && !e.blink_enabled());
        assert_eq!(e.particles.mode, ParticleMode::Railgun);
    }

    #[test]
    fn a_particle_mode_is_spelt_as_neovides() {
        let s: Settings =
            toml::from_str("[effects.particles]\nmode = \"pixiedust\"\n").expect("valid");
        assert_eq!(s.effects.particles.mode, ParticleMode::Pixiedust);
        assert_eq!(ParticleMode::Pixiedust.name(), "pixiedust");
    }

    /// Defined elsewhere, like the prefix: the prompt may show it before a
    /// config is read.
    #[test]
    fn the_default_command_is_the_one_the_launcher_uses() {
        assert_eq!(SessionSettings::default().command, crate::launch::DEFAULT);
    }

    #[test]
    fn a_partial_document_keeps_the_other_tables_defaults() {
        let s: Settings = toml::from_str("[session]\ncommand = \"nvim -u NONE --listen {sock}\"\n")
            .expect("valid");
        assert_eq!(s.session.command, "nvim -u NONE --listen {sock}");
        assert_eq!(s.keys, KeySettings::default());
    }

    #[test]
    fn a_partial_keys_table_keeps_the_other_keys_defaults() {
        let s: Settings = toml::from_str("[keys]\ntimeout_ms = 250\n").expect("valid");
        assert_eq!(s.keys.timeout_ms, 250);
        assert_eq!(s.keys.prefix, KeySettings::default().prefix);
    }

    #[test]
    fn a_remapped_prefix_parses_to_its_byte() {
        let s: Settings = toml::from_str("[keys]\nprefix = \"C-a\"\n").expect("valid");
        assert_eq!(s.keys.prefix, 0x01);
    }

    /// As a table and inline.
    #[test]
    fn a_partial_bind_table_moves_only_what_it_names() {
        let s: Settings = toml::from_str("[keys.bind]\ndetach = \"x\"\n").expect("valid");
        let mut want = crate::keys::Keymap::default();
        assert!(want.bind("detach", b'x'));
        assert_eq!(s.keys.bind, want);
        assert_eq!(s.keys.prefix, KeySettings::default().prefix);
        assert_eq!(s.keys.timeout_ms, KeySettings::default().timeout_ms);

        let inline: Settings =
            toml::from_str("[keys]\nbind = { detach = \"x\" }\n").expect("valid");
        assert_eq!(inline, s);
    }

    /// Both are named, including a default the file never mentions. Swapping
    /// two keys is fine.
    #[test]
    fn two_commands_on_one_key_are_refused_with_both_named() {
        for (doc, first, second) in [
            (
                "[keys.bind]\ndetach = \"c\"\n",
                "keys.bind.detach",
                "keys.bind.new",
            ),
            (
                "[keys.bind]\nnext = \"z\"\nhelp = \"z\"\n",
                "keys.bind.next",
                "keys.bind.help",
            ),
        ] {
            match parse(Path::new("test.toml"), doc).expect_err(doc) {
                ConfigError::Invalid { message, .. } => assert!(
                    message.contains(first) && message.contains(second),
                    "{doc:?} -> {message:?}"
                ),
                other => panic!("{doc:?}: expected Invalid, got {other:?}"),
            }
        }

        let swapped = parse(
            Path::new("test.toml"),
            "[keys.bind]\ndetach = \"c\"\nnew = \"d\"\n",
        )
        .expect("a swap");
        assert_eq!(
            swapped.keys.bind.command(b'c'),
            Some(crate::keys::Action::Detach)
        );
        assert_eq!(
            swapped.keys.bind.command(b'd'),
            Some(crate::keys::Action::Create)
        );
    }

    /// Each error points at its own line.
    #[test]
    fn a_bad_binding_says_what_would_do() {
        let doc = "[keys.bind]\nnext = \"j\"\ndetatch = \"x\"\n";
        let msg = toml::from_str::<Settings>(doc).expect_err(doc).to_string();
        assert!(
            msg.contains("unknown field `detatch`") && msg.contains("`detach`"),
            "{msg}"
        );
        assert!(msg.contains("line 3"), "not at the name: {msg}");

        let doc = "[keys.bind]\nnext = \"j\"\ndetach = \"dd\"\n";
        let msg = toml::from_str::<Settings>(doc).expect_err(doc).to_string();
        assert!(msg.contains("one character"), "{msg}");
        assert!(msg.contains("line 3"), "not at the key: {msg}");
    }

    #[test]
    fn a_typo_is_never_silently_ignored() {
        for (what, doc) in [
            ("an unknown top-level table", "[colours]\nx = 1\n"),
            ("an unknown keys key", "[keys]\nprefx = \"C-a\"\n"),
            ("an unparseable prefix", "[keys]\nprefix = \"nope\"\n"),
            ("an unknown command", "[keys.bind]\ndetatch = \"x\"\n"),
            ("a command outside [keys.bind]", "[keys]\ndetach = \"x\"\n"),
            ("a key of two characters", "[keys.bind]\ndetach = \"dd\"\n"),
            ("a digit for a key", "[keys.bind]\ndetach = \"1\"\n"),
            ("a chord for a key", "[keys.bind]\ndetach = \"C-d\"\n"),
            ("a key as a number", "[keys.bind]\ndetach = 1\n"),
            ("a key where its table goes", "[keys]\nbind = \"x\"\n"),
            ("an unknown session key", "[session]\ncmd = \"nvim\"\n"),
            ("an unknown fade key", "[effects.fade]\nduration = 40\n"),
            ("an unknown client key", "[client]\nkeep = true\n"),
            ("an unknown style key", "[style]\ncolour = \"#7aa2f7\"\n"),
            ("a colour by name", "[style]\ncolor = \"blue\"\n"),
            ("a colour with no #", "[style]\ncolor = \"7aa2f7\"\n"),
            ("a colour as a number", "[style]\ncolor = 0x7aa2f7\n"),
            (
                "a colour of the wrong length",
                "[style]\ncolor = \"#7aa2f\"\n",
            ),
            (
                "the colour outside [style]",
                "[color]\ncolor = \"#7aa2f7\"\n",
            ),
            (
                "the table [style] was called",
                "[theme]\ncolor = \"#7aa2f7\"\n",
            ),
            ("an unknown effect", "[effects.sparkles]\nenabled = true\n"),
            (
                "an unknown cursor key",
                "[effects.cursor]\nenable = false\n",
            ),
            (
                "an unknown attach key",
                "[effects.attach]\nenable = false\n",
            ),
            ("an unknown kill key", "[effects.kill]\nenable = false\n"),
            (
                "an unknown filter key",
                "[effects.filter]\nenable = false\n",
            ),
            (
                "an unparseable filter value",
                "[effects.filter]\nenabled = 1\n",
            ),
            ("an unknown effects key", "[effects]\nenable = false\n"),
            ("an unknown move key", "[effects.move]\nenable = false\n"),
            (
                "an unparseable move value",
                "[effects.move]\nenabled = \"no\"\n",
            ),
            ("an unparseable client value", "[client]\nper_session = 1\n"),
            ("an unparseable lazy value", "[client]\nlazy = \"no\"\n"),
            (
                "an unparseable prediction switch",
                "[client]\npredict_scrolls = \"yes\"\n",
            ),
            (
                "an unparseable fade value",
                "[effects.fade]\nenabled = \"yes\"\n",
            ),
            (
                "an effect's table outside [effects]",
                "[fade]\nenabled = false\n",
            ),
            ("a client nobody makes", "[client]\nui = \"vim\"\n"),
            (
                "an unknown smear key",
                "[effects.smear]\ntrail_size = 1.0\n",
            ),
            (
                "a particle mode Neovide does not have",
                "[effects.particles]\nmode = \"sparkles\"\n",
            ),
            (
                "an unparseable particle value",
                "[effects.particles]\nenabled = \"yes\"\n",
            ),
            // How the client's effects look is not the file's to say.
            ("a smear's look", "[effects.smear]\ntrail = 0.4\n"),
            ("a window's timing", "[effects.windows]\nopen_ms = 200\n"),
            ("a particle's look", "[effects.particles]\nspeed = 6.0\n"),
            ("an unknown scroll key", "[effects.scroll]\nlength = 1\n"),
            (
                "an unknown windows key",
                "[effects.windows]\nenable = true\n",
            ),
            ("an unknown blink key", "[effects.blink]\nsmooth = true\n"),
        ] {
            assert!(
                toml::from_str::<Settings>(doc).is_err(),
                "{what} should be rejected: {doc:?}"
            );
        }
    }

    /// The timeout feeds `poll` and the fade a `sleep`, so both are bounded;
    /// the extremes that pass are pinned to keep the bounds generous.
    #[test]
    fn out_of_range_values_are_rejected_with_their_key_named() {
        let cases = [
            ("[keys]\ntimeout_ms = 0\n", "keys.timeout_ms"),
            ("[keys]\ntimeout_ms = 60001\n", "keys.timeout_ms"),
            // Past `c_int`: the value `poll` would have read as "block forever".
            ("[keys]\ntimeout_ms = 3000000000\n", "keys.timeout_ms"),
            (
                "[effects.fade]\nduration_ms = 0\n",
                "effects.fade.duration_ms",
            ),
            (
                "[effects.fade]\nduration_ms = 2001\n",
                "effects.fade.duration_ms",
            ),
        ];
        for (doc, key) in cases {
            let err = parse(Path::new("test.toml"), doc).expect_err(doc);
            match err {
                ConfigError::Invalid { message, .. } => {
                    assert!(message.contains(key), "{doc:?} -> {message:?}")
                }
                other => panic!("{doc:?}: expected Invalid, got {other:?}"),
            }
        }
        for doc in [
            "[keys]\ntimeout_ms = 1\n",
            "[keys]\ntimeout_ms = 60000\n",
            "[effects.fade]\nduration_ms = 1\n",
            "[effects.fade]\nduration_ms = 2000\n",
        ] {
            parse(Path::new("test.toml"), doc).expect(doc);
        }
    }

    #[test]
    fn an_unusable_command_is_rejected_with_its_key_named() {
        for doc in [
            "[session]\ncommand = \"nvim --headless\"\n",
            "[session]\ncommand = \"\"\n",
            "[session]\ncommand = \"nvim --listen '{sock}\"\n",
        ] {
            let err = parse(Path::new("test.toml"), doc).expect_err(doc);
            match err {
                ConfigError::Invalid { message, .. } => {
                    assert!(
                        message.contains("session.command"),
                        "{doc:?} -> {message:?}"
                    )
                }
                other => panic!("{doc:?}: expected Invalid, got {other:?}"),
            }
        }
    }

    /// `main` prints with anyhow's `{:#}`, which appends every `source()`; the
    /// TOML error is already in the message.
    #[test]
    fn a_parse_error_is_printed_once() {
        let doc = "[keys]\nprefx = \"C-a\"\n";
        let err = parse(Path::new("config.toml"), doc).expect_err(doc);
        let cause = match &err {
            ConfigError::Parse { error, .. } => error.to_string(),
            other => panic!("expected Parse, got {other:?}"),
        };
        let plain = err.to_string();
        let printed = format!("{:#}", anyhow::Error::from(err));
        assert_eq!(printed.matches(&cause).count(), 1, "{printed}");
        assert_eq!(printed, format!("config.toml: {cause}"));
        assert_eq!(printed, plain, "printed plainly, it reads the same");
    }

    // --- first-run template (pure) -----------------------------------------

    #[test]
    fn the_default_config_round_trips_to_the_chosen_prefix() {
        for byte in [crate::keys::PREFIX, 0x01, 0x02] {
            let rendered = render_default_config(byte);
            let s: Settings = toml::from_str(&rendered).expect("valid toml");
            assert_eq!(s.keys.prefix, byte, "prefix survives the round trip");
            // Only the prefix is active; everything else stays at the defaults.
            assert_eq!(s, with_prefix(byte));
            s.validate().expect("valid");
        }
    }

    #[test]
    fn the_default_config_activates_only_the_prefix() {
        let rendered = render_default_config(0x01);
        assert!(
            rendered.contains("\nprefix = \"Ctrl-a\" "),
            "the chosen prefix is the one active setting: {rendered:?}"
        );
        // The timeout, the command and the fade are documentation, not active
        // settings.
        assert!(rendered.contains("# timeout_ms = 1000"));
        // So is every command's key, one line each in table order.
        assert!(rendered.contains("\n[keys.bind]   # "), "{rendered:?}");
        let mut rest = rendered.as_str();
        for b in crate::keys::BINDINGS {
            let line = format!(
                "\n# {:<name$} = {:?}",
                b.hint,
                crate::keys::key_label(b.key),
                name = command_width()
            );
            let at = rest.find(&line).unwrap_or_else(|| {
                panic!("the template must document {line:?}, in order: {rendered:?}")
            });
            rest = &rest[at + line.len()..];
        }
        assert!(
            rendered.contains("# command = \"nvim --headless --listen {sock}\""),
            "the template must document the command: {rendered:?}"
        );
        assert!(rendered.contains("\n[client]\n"), "{rendered:?}");
        for table in [
            "[style]",
            "[effects]",
            "[effects.fade]",
            "[effects.move]",
            "[effects.cursor]",
            "[effects.attach]",
            "[effects.kill]",
            "[effects.filter]",
            "[effects.create]",
            "[effects.smear]",
            "[effects.particles]",
            "[effects.scroll]",
            "[effects.windows]",
            "[effects.blink]",
        ] {
            assert!(
                rendered.contains(&format!("\n{table}\n")),
                "the template must have {table}: {rendered:?}"
            );
        }
        assert_eq!(
            rendered.matches("\n# enabled = true ").count(),
            9,
            "the master switch and each switch-only effect on by default: {rendered:?}"
        );
        assert_eq!(
            rendered.matches("\n# enabled = false ").count(),
            3,
            "the cursor's travel, the particles and the smooth blink: {rendered:?}"
        );
        for line in [
            "# per_session     = true",
            "# lazy            = false",
            "# ui              = \"nvmux\"",
            "# predict_scrolls = \"auto\"",
            "# predict_motions = \"auto\"",
            "# predict_typing  = \"auto\"",
            "# centre          = \"off\"",
            "# window_width    = 120",
            "# color = \"terminal\"",
            "# float = true ",
            "# enabled     = true",
            "# duration_ms = 200",
            "# session     = true",
            "# mode    = \"railgun\"",
        ] {
            assert!(
                rendered.contains(line),
                "the template must document {line:?}: {rendered:?}"
            );
        }
    }

    /// Each commented line, uncommented with its note, must parse to its
    /// default, and together they must be exactly the README's settings.
    #[test]
    fn the_default_config_uncommented_is_every_setting_at_its_default() {
        let rendered = render_default_config(crate::keys::PREFIX);
        // A commented setting is a key — lowercase and underscores — before
        // ` = `. The header and the prose are not.
        let is_setting = |rest: &&str| {
            rest.split_once(" = ").is_some_and(|(key, _)| {
                let key = key.trim_end();
                !key.is_empty() && key.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
            })
        };
        let mut uncommented = String::new();
        for line in rendered.lines() {
            uncommented.push_str(line.strip_prefix("# ").filter(is_setting).unwrap_or(line));
            uncommented.push('\n');
        }
        let s: Settings = toml::from_str(&uncommented).expect("uncommented, it parses");
        assert_eq!(s, Settings::default(), "{uncommented}");
        let ours: toml::Table = toml::from_str(&uncommented).expect("valid toml");
        let readme: toml::Table = toml::from_str(readme_settings()).expect("valid toml");
        assert_eq!(ours, readme, "not the README's settings: {uncommented}");
    }

    /// `remote` included; settings other than `[client]` and `[effects]` come
    /// back as defaults.
    #[test]
    fn what_a_client_draws_with_survives_its_environment() {
        let mut settings = Settings::default();
        settings.client.ui = Ui::Nvim;
        settings.client.predict_scrolls = Predict::Off;
        settings.client.predict_typing = Predict::On;
        settings.client.centre = Centre::Fixed;
        settings.client.window_width = 0;
        settings.effects.enabled = false;
        settings.effects.blink.enabled = true;
        settings.keys.timeout_ms = 1500;
        assert!(!settings.drawing().remote, "nothing has said otherwise");
        let drawing = Drawing {
            remote: true,
            ..settings.drawing()
        };
        assert_eq!(Drawing::from_env(&drawing.to_env()), Some(drawing));
        assert_eq!(drawing.settings().client, settings.client);
        assert_eq!(drawing.settings().effects, settings.effects);
        assert_eq!(
            drawing.settings().keys,
            KeySettings::default(),
            "not a client's"
        );
        assert_eq!(Drawing::from_env("{"), None);
    }

    // --- live settings (each test its own store) ---------------------------

    fn generation_of(store: &Store) -> u64 {
        store.generation.load(Ordering::SeqCst)
    }

    /// `saved` is unchanged until a save.
    #[test]
    fn a_change_is_what_get_hands_out_and_counts_a_generation() {
        let store = Store::new();
        store.init(Settings::default());
        let mut changed = Settings::default();
        changed.keys.timeout_ms = 1500;
        changed.effects.smear.enabled = true;

        store.apply(changed.clone()).expect("valid");
        assert_eq!(*store.get(), changed);
        assert_eq!(generation_of(&store), 1);
        assert_eq!(*store.saved(), Settings::default(), "the file is as it was");

        store.apply(changed.clone()).expect("valid");
        assert_eq!(
            generation_of(&store),
            1,
            "the same settings again are no change"
        );

        store.set_saved(changed.clone());
        assert_eq!(*store.saved(), changed);
        assert_eq!(generation_of(&store), 1, "a save changes nothing that runs");
    }

    /// Same message as a file would get, and nothing changes.
    #[test]
    fn a_change_a_file_would_be_refused_for_is_refused() {
        let store = Store::new();
        store.init(Settings::default());
        let mut zero = Settings::default();
        zero.keys.timeout_ms = 0;
        let err = store.apply(zero).expect_err("a zero timeout");
        assert!(err.contains("keys.timeout_ms"), "{err}");

        let mut clash = Settings::default();
        assert!(clash.keys.bind.bind("detach", b'c'));
        let err = store.apply(clash).expect_err("two commands on one key");
        assert!(err.contains("keys.bind.detach"), "{err}");

        assert_eq!(*store.get(), Settings::default());
        assert_eq!(generation_of(&store), 0);
    }

    /// `init` sets both live and saved settings; a second call changes neither.
    #[test]
    fn a_second_init_keeps_the_first() {
        let store = Store::new();
        let mut first = Settings::default();
        first.client.lazy = !first.client.lazy;
        store.init(first.clone());
        store.init(Settings::default());
        assert_eq!(*store.get(), first);
        assert_eq!(*store.saved(), first);
    }

    // --- saving (pure) ------------------------------------------------------

    /// The first-run template with the default prefix.
    fn template() -> String {
        render_default_config(crate::keys::PREFIX)
    }

    /// `settings` with the row at `path` set to `value`.
    fn with(settings: &Settings, path: &str, value: crate::settings::Value) -> Settings {
        let mut next = settings.clone();
        crate::settings::find(path)
            .unwrap_or_else(|| panic!("no setting {path}"))
            .set(&mut next, &value);
        next
    }

    /// Save `live` over `text`, the file having held `saved`.
    fn save_over(text: Option<&str>, saved: &Settings, live: &Settings) -> (String, usize) {
        saved_text(text, live, saved, false).expect("saved")
    }

    /// Lines added and removed by a save.
    fn diff(before: &str, after: &str) -> (Vec<String>, Vec<String>) {
        let was: Vec<&str> = before.lines().collect();
        let now: Vec<&str> = after.lines().collect();
        (
            now.iter()
                .filter(|l| !was.contains(l))
                .map(|l| l.to_string())
                .collect(),
            was.iter()
                .filter(|l| !now.contains(l))
                .map(|l| l.to_string())
                .collect(),
        )
    }

    use crate::settings::Value;

    #[test]
    fn a_setting_the_template_comments_is_uncommented_in_place() {
        let before = template();
        let live = with(&Settings::default(), "keys.timeout_ms", Value::Number(1500));
        let (after, count) = save_over(Some(&before), &Settings::default(), &live);
        assert_eq!(count, 1);
        let (added, removed) = diff(&before, &after);
        // The note keeps its column; the template pads the number to six.
        assert_eq!(
            added,
            vec!["timeout_ms = 1500     # how long a lone prefix or half-typed number waits"]
        );
        assert_eq!(
            removed,
            vec!["# timeout_ms = 1000     # how long a lone prefix or half-typed number waits"]
        );
        assert_eq!(before.lines().count(), after.lines().count());
        assert_eq!(read_back(&after), Ok(live));
    }

    #[test]
    fn a_setting_back_at_its_default_is_commented_back_out() {
        let saved = with(
            &Settings::default(),
            "client.centre",
            Value::Choice("smart"),
        );
        let (before, _) = save_over(Some(&template()), &Settings::default(), &saved);
        assert!(
            before.contains("\ncentre          = \"smart\" "),
            "{before}"
        );

        let (after, count) = save_over(Some(&before), &saved, &Settings::default());
        assert_eq!(count, 1);
        assert_eq!(after, template(), "back to the template, byte for byte");
    }

    #[test]
    fn a_setting_the_file_sets_is_changed_where_it_is() {
        let before = "[keys]\nprefix = \"Ctrl-Space\"   # mine\n\n[client]\nlazy = true\n";
        let saved: Settings = toml::from_str(before).expect("valid");
        let live = with(&saved, "keys.prefix", Value::Prefix(0x01));
        let live = with(&live, "client.lazy", Value::Switch(false));
        let (after, count) = save_over(Some(before), &saved, &live);
        assert_eq!(count, 2);
        assert_eq!(
            after,
            "[keys]\nprefix = \"Ctrl-a\"       # mine\n\n[client]\n# lazy = false\n"
        );
    }

    #[test]
    fn with_no_file_the_template_is_written_with_the_change() {
        let live = with(
            &Settings::default(),
            "effects.smear.enabled",
            Value::Switch(true),
        );
        let (after, count) = save_over(None, &Settings::default(), &live);
        assert_eq!(count, 1);
        let (added, _) = diff(&template(), &after);
        assert_eq!(
            added,
            vec!["enabled = true    # the cursor travels between cells rather than jumping"]
        );
        assert_eq!(read_back(&after), Ok(live));

        let (nothing, count) = save_over(None, &Settings::default(), &Settings::default());
        assert_eq!(
            (nothing.as_str(), count),
            ("", 0),
            "and nothing, with nothing changed"
        );
    }

    /// A missing table is added at the end.
    #[test]
    fn a_setting_with_no_line_is_added_to_its_table() {
        let before =
            "[client]\nlazy = false\n# a note about the client\n\n[style]\ncolor = \"#7aa2f7\"";
        let saved: Settings = toml::from_str(before).expect("valid");
        let live = with(&saved, "client.centre", Value::Choice("smart"));
        let live = with(&live, "effects.blink.enabled", Value::Switch(true));
        let live = with(&live, "client.window_width", Value::Number(100));
        let (after, count) = save_over(Some(before), &saved, &live);
        assert_eq!(count, 3);
        assert_eq!(
            after,
            "[client]\nlazy = false\ncentre = \"smart\"\nwindow_width = 100\n# a note about the client\n\n\
             [style]\ncolor = \"#7aa2f7\"\n\n[effects.blink]\nenabled = true\n"
        );
        assert_eq!(read_back(&after), Ok(live));
    }

    /// With comments, an inline table and dotted keys.
    #[test]
    fn a_hand_written_file_keeps_everything_a_save_did_not_change() {
        let before = "\
# My nvmux.
[keys]
bind = { detach = \"x\", new = \"N\" }   # moved off d and c

[effects]
fade.enabled = false   # too slow over ssh
fade.session = false
";
        let saved: Settings = toml::from_str(before).expect("valid");
        let live = with(&saved, "keys.bind.detach", Value::Key(b'z'));
        let live = with(&live, "effects.fade.session", Value::Switch(true));
        let (after, count) = save_over(Some(before), &saved, &live);
        assert_eq!(count, 2);
        assert_eq!(
            after,
            "\
# My nvmux.
[keys]
bind = { detach = \"z\", new = \"N\" }   # moved off d and c

[effects]
fade.enabled = false   # too slow over ssh
# fade.session = true
"
        );

        // A table spelt in dotted keys takes a setting it did not have.
        let live = with(&saved, "effects.fade.duration_ms", Value::Number(100));
        let (after, count) = save_over(Some(before), &saved, &live);
        assert_eq!(count, 1);
        assert_eq!(read_back(&after), Ok(live));
        for kept in ["# My nvmux.", "# moved off d and c", "# too slow over ssh"] {
            assert!(after.contains(kept), "lost {kept:?}: {after}");
        }
    }

    #[test]
    fn a_table_there_only_for_one_under_it_gets_a_header() {
        let before = "[effects.fade]\nenabled = false\n";
        let saved: Settings = toml::from_str(before).expect("valid");
        let live = with(&saved, "effects.enabled", Value::Switch(false));
        let (after, _) = save_over(Some(before), &saved, &live);
        assert_eq!(
            after,
            "[effects.fade]\nenabled = false\n\n[effects]\nenabled = false\n"
        );
        assert_eq!(read_back(&after), Ok(live));
    }

    /// A file change to the very value being saved is no clash.
    #[test]
    fn a_setting_the_file_changed_too_is_overwritten_only_when_asked() {
        let saved = Settings::default();
        let live = with(&saved, "keys.timeout_ms", Value::Number(1500));
        let edited = "[keys]\ntimeout_ms = 2000\n";
        assert_eq!(
            saved_text(Some(edited), &live, &saved, false),
            Err(Refusal::Changed(vec!["keys.timeout_ms".into()]))
        );
        let (after, count) = saved_text(Some(edited), &live, &saved, true).expect("overwritten");
        assert_eq!((after.as_str(), count), ("[keys]\ntimeout_ms = 1500\n", 1));

        let same = "[keys]\ntimeout_ms = 1500\n";
        let (after, count) = saved_text(Some(same), &live, &saved, false).expect("saved");
        assert_eq!((after.as_str(), count), (same, 0), "already so");

        // A hand edit to another setting is no clash, and stays.
        let other = "[client]\nlazy = true\n";
        let (after, _) = saved_text(Some(other), &live, &saved, false).expect("saved");
        assert_eq!(
            after,
            "[client]\nlazy = true\n\n[keys]\ntimeout_ms = 1500\n"
        );
    }

    /// The refusal says where parsing stopped.
    #[test]
    fn a_file_that_no_longer_reads_is_not_written_over() {
        let live = with(&Settings::default(), "client.lazy", Value::Switch(true));
        for (text, line) in [
            ("[keys]\nprefx = \"C-a\"\n", "line 2:"),
            ("[keys\n", "line 1:"),
        ] {
            match saved_text(Some(text), &live, &Settings::default(), true) {
                Err(Refusal::Unreadable(why)) => assert!(why.starts_with(line), "{why}"),
                other => panic!("{text:?}: {other:?}"),
            }
        }
        match saved_text(
            Some("[keys]\ntimeout_ms = 0\n"),
            &live,
            &Settings::default(),
            true,
        ) {
            Err(Refusal::Unreadable(why)) => assert!(why.contains("keys.timeout_ms"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn every_setting_saves_and_saves_back_to_the_template() {
        for setting in crate::settings::all() {
            let changed = match setting.default_value() {
                Value::Switch(on) => Value::Switch(!on),
                Value::Number(n) => Value::Number(n + 1),
                Value::Choice(name) => {
                    let crate::settings::Kind::Choice { options, .. } = setting.kind else {
                        unreachable!()
                    };
                    Value::Choice(options.iter().find(|o| **o != name).expect("another"))
                }
                Value::Prefix(_) => Value::Prefix(0x14),
                Value::Key(_) => Value::Key(b'Z'),
                // Short enough for its note to keep its column both ways.
                Value::Text(_) => Value::Text("nvim -u NONE --listen {sock}".into()),
                Value::Colour(_) => Value::Colour(ThemeColor::Mono(Rgb(1, 2, 3))),
            };
            let live = with(&Settings::default(), &setting.path(), changed);
            let (after, count) = save_over(Some(&template()), &Settings::default(), &live);
            assert_eq!(count, 1, "{}", setting.path());
            assert_eq!(
                read_back(&after).as_ref(),
                Ok(&live),
                "{}: {after}",
                setting.path()
            );
            let (back, _) = save_over(Some(&after), &live, &Settings::default());
            assert_eq!(
                read_back(&back),
                Ok(Settings::default()),
                "{}",
                setting.path()
            );
            // The prefix is the template's one active line; back at its
            // default it is commented out like any other.
            let want = match setting.path().as_str() {
                "keys.prefix" => template().replace("\nprefix = ", "\n# prefix = "),
                _ => template(),
            };
            assert_eq!(back, want, "{}", setting.path());
        }
    }

    /// Written as a TOML literal string, which needs no escapes.
    #[test]
    fn a_command_with_quotes_reads_back_as_typed() {
        let typed = "nvim -u \"my init.lua\" --listen {sock}";
        let live = with(
            &Settings::default(),
            "session.command",
            Value::Text(typed.into()),
        );
        let (after, _) = save_over(Some(&template()), &Settings::default(), &live);
        assert!(
            after.contains(
                r#"command = 'nvim -u "my init.lua" --listen {sock}' # {sock} is required"#
            ),
            "{after}"
        );
        assert_eq!(
            read_back(&after).map(|s| s.session.command),
            Ok(typed.to_string())
        );
    }

    #[test]
    fn a_file_with_crlf_line_endings_keeps_them() {
        let before = "[client]\r\nlazy = true    # mine\r\n";
        let saved: Settings = toml::from_str(before).expect("valid");
        let live = with(&saved, "client.lazy", Value::Switch(false));
        let (after, _) = save_over(Some(before), &saved, &live);
        assert_eq!(after, "[client]\r\n# lazy = false   # mine\r\n");
    }

    // --- path resolution (pure) --------------------------------------------

    #[test]
    fn resolve_prefers_nvmux_config_then_xdg_then_home() {
        let nvmux = OsStr::new("/explicit.toml");
        let xdg = OsStr::new("/xdg");
        let home = OsStr::new("/home/u");

        assert_eq!(
            resolve_config_path(Some(nvmux), Some(xdg), Some(home)),
            ConfigSource::Explicit(PathBuf::from("/explicit.toml"))
        );
        assert_eq!(
            resolve_config_path(None, Some(xdg), Some(home)),
            ConfigSource::Default(PathBuf::from("/xdg/nvmux/config.toml"))
        );
        assert_eq!(
            resolve_config_path(None, None, Some(home)),
            ConfigSource::Default(PathBuf::from("/home/u/.config/nvmux/config.toml"))
        );
        assert_eq!(resolve_config_path(None, None, None), ConfigSource::None);
    }

    #[test]
    fn an_empty_env_value_is_treated_as_unset() {
        let empty = OsStr::new("");
        let home = OsStr::new("/home/u");
        assert_eq!(
            resolve_config_path(Some(empty), Some(empty), Some(home)),
            ConfigSource::Default(PathBuf::from("/home/u/.config/nvmux/config.toml"))
        );
    }

    // --- load() (touches process-global env; guarded) ----------------------

    /// `load` reads `HOME`/`XDG_CONFIG_HOME`/`NVMUX_CONFIG`, which are
    /// process-global. No other test touches them, so this only serialises
    /// these tests.
    static ENV_GUARD: Mutex<()> = Mutex::new(());

    /// Unsets the three vars for a test and restores them on drop, even on
    /// panic.
    struct ScrubbedEnv {
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl ScrubbedEnv {
        fn new() -> Self {
            let saved = ["NVMUX_CONFIG", "XDG_CONFIG_HOME", "HOME"]
                .into_iter()
                .map(|k| (k, std::env::var_os(k)))
                .collect::<Vec<_>>();
            for (k, _) in &saved {
                std::env::remove_var(k);
            }
            Self { saved }
        }

        fn set(&self, key: &str, value: impl AsRef<OsStr>) {
            std::env::set_var(key, value);
        }
    }

    impl Drop for ScrubbedEnv {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        crate::test_support::scratch_dir(&format!("cfg-{tag}"))
    }

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn an_absent_default_file_yields_defaults() {
        let _lock = guard();
        let env = ScrubbedEnv::new();
        let home = scratch("absent-home");
        env.set("HOME", &home);

        assert_eq!(load().expect("no file is fine"), Settings::default());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn nvmux_config_reads_an_explicit_file() {
        let _lock = guard();
        let env = ScrubbedEnv::new();
        let dir = scratch("explicit");
        let file = dir.join("custom.toml");
        std::fs::write(&file, "[keys]\ntimeout_ms = 250\n").expect("write");
        env.set("NVMUX_CONFIG", &file);

        assert_eq!(load().expect("valid").keys.timeout_ms, 250);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn nvmux_config_set_to_a_missing_file_is_fatal() {
        let _lock = guard();
        let env = ScrubbedEnv::new();
        let missing = scratch("missing").join("nope.toml");
        env.set("NVMUX_CONFIG", &missing);

        assert!(matches!(load(), Err(ConfigError::ExplicitMissing(_))));
    }

    #[test]
    fn a_malformed_default_file_is_fatal() {
        let _lock = guard();
        let env = ScrubbedEnv::new();
        let home = scratch("malformed-home");
        let cfg = home.join(".config").join("nvmux");
        std::fs::create_dir_all(&cfg).expect("mkdir");
        std::fs::write(cfg.join("config.toml"), "[keys]\nprefx = \"C-a\"\n").expect("write");
        env.set("HOME", &home);

        assert!(
            matches!(load(), Err(ConfigError::Parse { .. })),
            "a typo in the default file must not be silently ignored"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    // --- first-run detection + write (touches env; guarded) ----------------

    #[test]
    fn first_run_target_is_none_when_nvmux_config_is_set() {
        let _lock = guard();
        let env = ScrubbedEnv::new();
        env.set("NVMUX_CONFIG", "/some/explicit.toml");
        env.set("HOME", "/home/whoever");
        assert_eq!(first_run_target(), None, "an explicit config is the user's");
    }

    #[test]
    fn first_run_target_points_at_the_default_when_absent() {
        let _lock = guard();
        let env = ScrubbedEnv::new();
        let home = scratch("first-run-home");
        env.set("HOME", &home);
        let want = home.join(".config").join("nvmux").join("config.toml");
        assert_eq!(first_run_target(), Some(want));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn first_run_target_is_none_once_a_file_exists() {
        let _lock = guard();
        let env = ScrubbedEnv::new();
        let home = scratch("first-run-existing");
        let cfg = home.join(".config").join("nvmux");
        std::fs::create_dir_all(&cfg).expect("mkdir");
        std::fs::write(cfg.join("config.toml"), "").expect("write");
        env.set("HOME", &home);
        assert_eq!(
            first_run_target(),
            None,
            "a present config is not first-run"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn xdg_config_home_wins_for_the_first_run_target() {
        let _lock = guard();
        let env = ScrubbedEnv::new();
        let xdg = scratch("first-run-xdg");
        let home = scratch("first-run-xdg-home");
        env.set("XDG_CONFIG_HOME", &xdg);
        env.set("HOME", &home);
        assert_eq!(
            first_run_target(),
            Some(xdg.join("nvmux").join("config.toml"))
        );
        std::fs::remove_dir_all(&xdg).ok();
        std::fs::remove_dir_all(&home).ok();
    }

    /// Including `$NVMUX_CONFIG`, which the first run never writes.
    #[test]
    fn a_save_writes_the_file_a_load_reads() {
        let _lock = guard();
        let env = ScrubbedEnv::new();
        let dir = scratch("save-explicit");
        let file = dir.join("mine.toml");
        std::fs::write(&file, "[client]\nlazy = true\n").expect("write");
        env.set("NVMUX_CONFIG", &file);

        let saved = load().expect("load");
        let live = with(&saved, "effects.smear.enabled", Value::Switch(true));
        assert_eq!(
            save(&live, &saved, false).expect("saved"),
            Saved {
                path: file.clone(),
                count: 1
            }
        );
        assert_eq!(load().expect("reload"), live);
        std::fs::remove_dir_all(&dir).ok();

        // With no $HOME there is nowhere to keep one.
        env.set("NVMUX_CONFIG", "");
        env.set("HOME", "");
        assert!(matches!(
            save(&live, &saved, false),
            Err(SaveError::Nowhere)
        ));
    }

    #[test]
    fn write_default_creates_the_dir_and_a_loadable_file() {
        let _lock = guard();
        let env = ScrubbedEnv::new();
        let home = scratch("write-default-home");
        env.set("HOME", &home);

        let target = first_run_target().expect("a first-run target");
        write_default(&target, 0x01).expect("write");
        assert!(target.exists(), "the config file was created");
        // It is no longer a first-run target, and load() reads the choice back.
        assert_eq!(first_run_target(), None);
        assert_eq!(load().expect("load").keys.prefix, 0x01);
        std::fs::remove_dir_all(&home).ok();
    }
}
