//! The user's configuration file.
//!
//! nvmux runs with no configuration at all; this module is how it *optionally*
//! reads one. The governing idea is that a config file only ever *overrides*:
//! an absent file, an empty file and an omitted field all reproduce the
//! built-in behaviour byte for byte, because they all resolve through the
//! per-table `Default` impls — which is where each default is written down,
//! once.
//!
//! # Strict, and loud
//!
//! A config file is edited by hand, so a typo is likely and silence is the wrong
//! response: [`deny_unknown_fields`] rejects an unknown key, a value that does
//! not parse is a hard error, and [`load`] surfaces all of it with the offending
//! path. The one asymmetry is *absence*: a file named explicitly by
//! `$NVMUX_CONFIG` must exist (asking for a file that is not there is a
//! mistake), while the default search paths may be absent and simply fall back
//! to the defaults.
//!
//! [`deny_unknown_fields`]: https://serde.rs/container-attrs.html#deny_unknown_fields
//!
//! # Loading
//!
//! [`load`] resolves the file (see `resolve_config_path`), reads and validates
//! it, and returns a [`Settings`]. `main` calls it once and hands the result to
//! [`init`]; everything else reads the process-global through [`get`]. `get`
//! falls back to [`Settings::default`] when nothing has been initialised, so a
//! unit test that never calls `init` reads the compiled defaults rather than a
//! developer's real `~/.config/nvmux/config.toml`.
//!
//! # Live
//!
//! What `get` hands out is the settings nvmux is running on *now*: the
//! settings screen ([`crate::ui::settings`]) changes them with [`apply`] while
//! nvmux runs, and every change counts a [`generation`], which is how a reader
//! that copied what it needed can tell its copy is stale. What the file holds
//! is kept beside them ([`saved`]), so that what differs between the two is
//! what is unsaved — and what a save writes. Each reader asks `get` when it
//! needs a value rather than holding one, so most of nvmux follows a change
//! with no more than that.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, RwLock};
use std::time::Duration;

use serde::Deserialize;

use crate::error::ConfigError;
use crate::palette::Rgb;

/// The whole configuration. Each field is a table of its own, so the file reads
/// as `[keys]`-style sections — `[keys.bind]` inside `[keys]` for the key after
/// the prefix that runs each command, `[theme]` for the colour nvmux's own
/// screens are drawn in, and `[effects]` with a table inside it for each
/// effect: `[effects.fade]`, `[effects.move]`, `[effects.cursor]`,
/// `[effects.attach]`, `[effects.kill]`,
/// `[effects.filter]` and `[effects.create]` for nvmux's own screens, and
/// `[effects.smear]`, `[effects.particles]`, `[effects.scroll]`,
/// `[effects.windows]` and `[effects.blink]` for nvmux's own client
/// (`[client] ui = "nvmux"`, see [`crate::client`]).
///
/// Not `Copy`: `[session] command` owns a `String`. Nothing reads it by value —
/// [`get`] hands out an `Arc` of it — so this costs nothing.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub keys: KeySettings,
    pub session: SessionSettings,
    pub client: ClientSettings,
    pub theme: ThemeSettings,
    pub effects: EffectsSettings,
}

/// The prefix key, how long a half-typed sequence waits, and which key after
/// the prefix runs which command (see [`crate::keys`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KeySettings {
    /// The prefix, written as `"C-Space"` / `"Ctrl-a"` in the file and parsed
    /// to its control byte by [`crate::keys::parse_prefix`].
    #[serde(deserialize_with = "de_prefix")]
    pub prefix: u8,
    /// How long to wait for the second byte of a prefix sequence before
    /// deciding the user meant a literal `<prefix>`, and how long a half-typed
    /// session number waits for another digit.
    pub timeout_ms: u64,
    /// `[keys.bind]`: the key after the prefix that runs each command, by the
    /// command's name on the hint bar — `detach = "x"`. A command left out
    /// stays on its own key (see [`crate::keys::Keymap`]); a key is read by
    /// [`crate::keys::parse_key`], and two commands on one key are refused at
    /// startup.
    pub bind: crate::keys::Keymap,
}

/// What a new session launches, when the user has not said otherwise in the
/// prompt and nothing has been remembered from a previous one.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionSettings {
    /// The command line, with `{sock}` standing in for the session's socket.
    /// Parsed by [`crate::launch::Launch`], which is also what rejects a bad one.
    pub command: String,
}

/// The client that draws a session, on the pty nvmux relays (see
/// [`crate::pty`]): nvmux's own, `nvmux --client`, unless `ui` says
/// Neovim's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClientSettings {
    /// Which client draws a session.
    ///
    /// `"nvmux"`, the default, is nvmux's own client ([`crate::client`]),
    /// which draws the editor itself and so can animate it — the scroll,
    /// windows moving, and if they are turned on the cursor travelling, its
    /// particles and its blink — under `[effects.smear]`,
    /// `[effects.particles]`, `[effects.scroll]`, `[effects.windows]` and
    /// `[effects.blink]`. `"nvim"` is Neovim's own TUI: everything the
    /// terminal can do reaches the editor exactly as Neovim negotiated it,
    /// because Neovim did the negotiating.
    pub ui: Ui,
    /// Keep one client per session rather than one in all.
    ///
    /// Off, the client in front is the only one there is: a switch retires it
    /// and starts another for the session switched to — a fork, a probe, and
    /// a first paint the new client waits for its server to send. On, a client
    /// that leaves the front is parked instead — read on a thread of its own,
    /// still attached to its server — and a switch back to its session puts
    /// the screen it last had straight back on the terminal and asks the
    /// server for its own on top: no fork, no probe, and nothing to wait for
    /// (see [`crate::pty::Parked`]).
    ///
    /// On by default. What it costs, each for as long as nvmux runs, and so
    /// the reasons to turn it off:
    ///
    /// - A parked client is still a UI of its session's server. Another UI on
    ///   the same session — another nvmux, another machine — shares its grid
    ///   with it, at the smaller of the two sizes; a second nvmux of your own
    ///   holds each session at the size its terminal had when it last left.
    /// - A switch no longer fires `UILeave` in the session it leaves, nor
    ///   `UIEnter` in the one it comes back to.
    /// - Every session visited keeps an idle `nvim` client (about 1.5 MB of
    ///   its own), a thread and a copy of its screen in nvmux, and its ssh
    ///   forward. A session that keeps redrawing — a `:terminal` running
    ///   something — keeps sending that client frames, over the link for a
    ///   remote one.
    /// - Nothing limits how many are kept: one per session visited.
    /// - The screen a switch back puts up is nvmux's copy, which has no
    ///   undercurl, underline colour, strikethrough, blink, conceal or
    ///   overline (see [`crate::shadow`]) until the server's own repaint
    ///   lands, a round trip or two later.
    pub per_session: bool,
    /// Start a session's client on its first visit, rather than every
    /// session's at once as nvmux starts.
    ///
    /// Off, the default, nvmux starts one for every session there is before
    /// the picker goes up, and holds each out of sight until its session is
    /// first visited (see [`crate::pty::Attachment::start_ahead`]). That
    /// visit then waits for nothing: what the client drew meanwhile goes onto
    /// the terminal as it was written, its questions to the terminal with it,
    /// and the terminal's answers come back to it then. On, a session has no
    /// client until it is first attached to — a fork, a probe, and a first
    /// paint to wait for, as on a switch to it with `per_session` off. A
    /// session created after nvmux started is started on its first visit
    /// either way.
    ///
    /// What off costs, and so the reasons to turn it on:
    ///
    /// - Every session has a client — a UI of its server, with all that
    ///   `per_session` says that costs — from the moment nvmux starts, not
    ///   from its first visit.
    /// - nvmux's start pays a listing and, per session, a fork and an ssh
    ///   forward, before the picker is drawn.
    /// - Every session is probed as nvmux starts, which ends a hit-enter
    ///   prompt in each, as a visit would (see `pty::probe_on`).
    /// - A client that writes more than [`crate::pty::AHEAD_MAX`] before its
    ///   first visit — a `:terminal` running something — is let go, and its
    ///   session is started on its first visit after all.
    ///
    /// With `per_session` off as well, a client started ahead is retired when
    /// it first leaves the front, like every other.
    pub lazy: bool,
    /// Scroll ahead of a slow link: a key that scrolls — `<C-e>`, `<C-d>`,
    /// `<C-f>`, `zz`, `zl` and their kind, in Normal mode or Insert — or a
    /// turn of the mouse wheel, either way, scrolls the window at once,
    /// before Neovim has heard of it, once the round trip to
    /// the session is long enough to be worth it — about 30 ms, which a
    /// session on this machine never is. nvmux's own client only (see
    /// [`crate::client::predict`]).
    ///
    /// What it costs: the rows a scroll uncovers are drawn without their
    /// signs or virtual text until Neovim's own arrive, a round trip later,
    /// and in the colours tree-sitter and highlights on the buffer give them
    /// but not `:syntax`'s; and the agent the client leaves in the editor
    /// sends the text of the lines around each window's view over the link,
    /// and those colours, as the view moves — more of them the slower the
    /// link. Off, every scroll waits for Neovim.
    pub predict: bool,
    /// On a terminal wider than a window's text needs, draw the text in the
    /// middle of the window: a window wider than its text wants has it that
    /// wide, its lines wrapping there and its status line's words standing
    /// over it. A window showing a terminal keeps its whole width: the
    /// program in it is told its window's width. Off, the default, every
    /// window's text is as wide as the window, as Neovim's own client draws
    /// it. nvmux's own client only (see `crate::client::wide`).
    pub centre: bool,
    /// How many columns a window's text gets with `centre` on, where
    /// `smart_width` does not judge otherwise. 0 for no limit, which is
    /// `centre` off whatever that says.
    pub window_width: u16,
    /// With `centre` on, judge how wide each window's text is to be by what
    /// it shows, rather than give every window `window_width`: its buffer's
    /// `'textwidth'` where it has one, and room for its long lines where they
    /// are not wrapped — all of them with `'wrap'` off, and with it on those
    /// of a Markdown table or code block. A window showing a terminal keeps
    /// its whole width either way (see `crate::client::wide`).
    pub smart_width: bool,
}

impl Default for ClientSettings {
    fn default() -> Self {
        Self {
            ui: Ui::Nvmux,
            per_session: true,
            lazy: false,
            predict: true,
            centre: false,
            window_width: 120,
            smart_width: true,
        }
    }
}

/// `[client] ui`: whose client draws a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ui {
    /// `nvim --remote-ui`: Neovim's own TUI.
    Nvim,
    /// `nvmux --client`: nvmux's own, animated (see [`crate::client`]).
    #[default]
    Nvmux,
}

impl Ui {
    /// Every client, in the order the settings screen steps through them.
    pub const ALL: [Ui; 2] = [Ui::Nvmux, Ui::Nvim];

    /// The client as the file spells it.
    pub fn name(self) -> &'static str {
        match self {
            Ui::Nvim => "nvim",
            Ui::Nvmux => "nvmux",
        }
    }
}

/// The colour nvmux's own screens are drawn in (see [`crate::theme`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThemeSettings {
    /// What nvmux draws its own screens in: the picker, the prompt, the help
    /// and the wait for a session, and over a session the name an attach
    /// hands across.
    ///
    /// `"terminal"`, the default, sets no colour at all, so the screens are in
    /// the terminal's own foreground on its own background, shaded with dim,
    /// bold and reverse. A colour, `"#rrggbb"` or `"#rgb"`, draws what they
    /// draw at full strength in that one instead: the names and what is
    /// typed, and the selection's bar, with the name standing out of it in the
    /// background's colour — every fade and effect over them taking it from
    /// that colour. What they draw dim — the session numbers, the notes, the
    /// key hints, the hint bar a prefix puts up — stays the terminal's own,
    /// dimmed, as it was. The background is left the terminal's own either
    /// way.
    ///
    /// Neovim's own screen is the editor's, and is never drawn in it. Nor is
    /// the first-run screen, which is up before there is a config to read
    /// this from. And `NO_COLOR` keeps every screen in the terminal's own
    /// colours, whatever this says.
    pub color: ThemeColor,
}

/// `[theme] color`: whose colour nvmux's own screens are drawn in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeColor {
    /// The terminal's own: no colour set, as nvmux has always drawn.
    #[default]
    Terminal,
    /// Shades of this one colour.
    Mono(Rgb),
}

impl ThemeColor {
    /// The value as the file spells it: `terminal`, or the colour as
    /// `#rrggbb`.
    pub fn name(self) -> String {
        match self {
            ThemeColor::Terminal => "terminal".to_string(),
            ThemeColor::Mono(Rgb(r, g, b)) => format!("#{r:02x}{g:02x}{b:02x}"),
        }
    }

    /// Read a value as the file spells it: `terminal`, or a colour as
    /// `#rrggbb` or `#rgb`, the digits in either case.
    ///
    /// Those two are the spellings of a colour a terminal can answer in that
    /// a person also writes, so they are read by the parser that reads the
    /// terminal's answers ([`crate::palette`]), and `#f80` means `#ff8800`
    /// there as it does here. Its other spellings — `rgb:ff/88/00`, and
    /// twelve digits — are a terminal's, and are refused here.
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

/// The colour comes in as a human string; [`ThemeColor::parse`] reads it, and
/// its message is folded into serde's error, which TOML then reports with the
/// offending span — the shape `de_prefix` gives the prefix.
impl<'de> Deserialize<'de> for ThemeColor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        ThemeColor::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// The animated effects: a master switch over them all, and a table of its own
/// for each — `[effects.fade]`, `[effects.move]`, `[effects.cursor]`,
/// `[effects.attach]`, `[effects.kill]`,
/// `[effects.filter]` and `[effects.create]`, and for nvmux's own client
/// `[effects.smear]`, `[effects.particles]`, `[effects.scroll]`,
/// `[effects.windows]` and `[effects.blink]` — with its own `enabled`. An effect runs only when both are on, which is what the
/// `*_enabled` methods answer, so nothing reads one switch without the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EffectsSettings {
    /// Master switch. Off, nothing animates, whatever the effects' own
    /// switches say.
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
    /// Whether the fade is switched on: its own switch and the master one.
    /// `NO_COLOR` and the terminal have their say after this (see
    /// [`crate::fade::enabled`]).
    pub fn fade_enabled(&self) -> bool {
        self.enabled && self.fade.enabled
    }

    /// Whether a session being moved streams stars off its name, the rows it
    /// trades places with step aside, and it lands: its own switch and the
    /// master one.
    pub fn move_enabled(&self) -> bool {
        self.enabled && self.moving.enabled
    }

    /// Whether the row the cursor leaves glows as it lets the bar go: its own
    /// switch and the master one. Whether it is drawn in colour or with a
    /// modifier is the terminal's say after this (see [`crate::ui::effects`]).
    pub fn cursor_enabled(&self) -> bool {
        self.enabled && self.cursor.enabled
    }

    /// Whether an attach from the picker hands the session's name across to
    /// the session: its own switch and the master one. It rides the fade's
    /// frames, so the fade decides the rest (see [`crate::handoff`]).
    pub fn attach_enabled(&self) -> bool {
        self.enabled && self.attach.enabled
    }

    /// Whether a kill is drawn — the name struck through while the `[y/N]`
    /// asks, and the row erased from its end once it is confirmed: its own
    /// switch and the master one.
    pub fn kill_enabled(&self) -> bool {
        self.enabled && self.kill.enabled
    }

    /// Whether rows a filter keystroke drops fade out rather than vanish: the
    /// filter's own switch and the master one.
    pub fn filter_enabled(&self) -> bool {
        self.enabled && self.filter.enabled
    }

    /// Whether `c` on the picker opens a gap where the new session will go
    /// before the prompt comes up: its own switch and the master one. Whether
    /// the picker then closes onto the gap or cuts away from it is the fade's
    /// say (see [`crate::ui::app::App::make_room`]).
    pub fn create_enabled(&self) -> bool {
        self.enabled && self.create.enabled
    }

    /// Whether nvmux's own client animates its cursor between cells: its own
    /// switch and the master one.
    pub fn smear_enabled(&self) -> bool {
        self.enabled && self.smear.enabled
    }

    /// Whether particles fly off the cursor as it moves: its own switch and
    /// the master one.
    pub fn particles_enabled(&self) -> bool {
        self.enabled && self.particles.enabled
    }

    /// Whether a scroll slides the text through the window: its own switch
    /// and the master one.
    pub fn scroll_enabled(&self) -> bool {
        self.enabled && self.scroll.enabled
    }

    /// Whether windows and floats move rather than jump: its own switch and
    /// the master one.
    pub fn windows_enabled(&self) -> bool {
        self.enabled && self.windows.enabled
    }

    /// Whether a blinking cursor fades in and out: its own switch and the
    /// master one.
    pub fn blink_enabled(&self) -> bool {
        self.enabled && self.blink.enabled
    }
}

// The client's effects are switches, and the particles' one choice of what
// they are: how each looks and how long it takes is the client's own (see
// `crate::client::anim`).

/// nvmux's own client: the cursor travelling between cells rather than
/// jumping, its leading edge ahead and its trailing edge behind, so that a
/// long move smears across the screen — Neovide's animated cursor (see
/// [`crate::client::anim::smear`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SmearSettings {
    /// This effect's own switch, under `[effects] enabled`. Off unless turned
    /// on; off, the cursor jumps, as the terminal's does.
    pub enabled: bool,
}

/// nvmux's own client: what flies off the cursor as it moves — Neovide's
/// `cursor_vfx_mode` (see [`crate::client::anim::vfx`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ParticleSettings {
    /// This effect's own switch, under `[effects] enabled`. Off by default,
    /// as Neovide's is.
    pub enabled: bool,
    pub mode: ParticleMode,
}

/// `[effects.particles] mode`, spelt as Neovide spells `cursor_vfx_mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
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
    /// Every mode, in the order the settings screen steps through them.
    pub const ALL: [ParticleMode; 6] = [
        ParticleMode::Railgun,
        ParticleMode::Torpedo,
        ParticleMode::Pixiedust,
        ParticleMode::Sonicboom,
        ParticleMode::Ripple,
        ParticleMode::Wireframe,
    ];

    /// The mode as the file spells it.
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

/// nvmux's own client: a scroll sliding the text through the window a row at
/// a time — Neovide's smooth scrolling (see
/// [`crate::client::anim::scroll`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScrollSettings {
    /// This effect's own switch, under `[effects] enabled`. Off, the text
    /// jumps to where it scrolled to.
    pub enabled: bool,
}

/// nvmux's own client: windows moving rather than jumping. A split window
/// opening, closing or changing size moves the way animate.nvim's window
/// module moves it, and one showing another buffer fades from one to the
/// other (see [`crate::client::anim::layout`] and
/// [`crate::client::anim::switch`]); a float, the message area or windows
/// rearranged slide to their new place as Neovide's do — its
/// `position_animation_length` (see [`crate::client::anim::motion`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WindowsSettings {
    /// This effect's own switch, under `[effects] enabled`.
    pub enabled: bool,
}

/// nvmux's own client: a blinking cursor fading out and back in rather than
/// flashing — Neovide's `cursor_smooth_blink` (see
/// [`crate::client::anim::blink`]). Only a block cursor fades; a bar or an
/// underline keeps the terminal's own blink.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BlinkSettings {
    /// This effect's own switch, under `[effects] enabled`. Off by default,
    /// as Neovide's is; and it does nothing for a cursor `'guicursor'` does
    /// not make blink.
    pub enabled: bool,
}

/// The fade between screens (see [`crate::fade`]): each one dissolves into the
/// terminal's own background colour and the next dissolves up out of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FadeSettings {
    /// This effect's own switch, under `[effects] enabled`. Two things force
    /// the fade off whatever either says: `NO_COLOR`, because the effect paints
    /// explicit colours, and a terminal that does not answer the startup colour
    /// query, because there is then nothing to fade *to* — see
    /// [`crate::fade::enabled`].
    pub enabled: bool,
    /// How long a dissolve takes, **both directions together**: a screen going
    /// out and the next one coming up divide this between them, half each (see
    /// [`FadeSettings::one_way`]). So a switch pays it once, for the screens:
    /// the card that says where it landed comes in with the session's screen
    /// (see [`crate::card`]), and the box that once said it, dissolving in and
    /// out around the second it was up, is not drawn (see
    /// [`crate::announce`]).
    ///
    /// Both directions rather than one because a transition is the thing
    /// anybody watching is timing: what the number names is how long the screen
    /// takes to change, not how long half of that takes. Must be at least 1 and
    /// at most `MAX_FADE_MS`.
    pub duration_ms: u64,
    /// Whether a Neovim screen dissolves too — in, once its first paint has
    /// settled, and out — rather than only nvmux's own screens. Costs a
    /// running parse of the session's output while it is attached (see
    /// [`crate::shadow`]); off, a session hard-cuts both ways. A kept client
    /// (`[client] per_session`, the default) parses the same output into its
    /// own copy of its screen either way, so off saves the parse only with
    /// `per_session = false` too.
    ///
    /// That parse is also what the hint bar puts back the row it covered from,
    /// so off, the bar blanks the row and waits for a repaint to fill it (see
    /// [`crate::hint`]).
    pub session: bool,
}

/// The stars a session's name streams while it is picked up to be moved, off
/// both ends of its row (see [`crate::ui::starfield`]); the rows it trades
/// places with on each move stepping aside, letting its bar go and throwing
/// sparks (see [`crate::ui::swap`]); and the impact when it is put down (see
/// [`crate::ui::landing`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MoveSettings {
    /// This effect's own switch, under `[effects] enabled`. Off, a session in
    /// flight is drawn still, trades places with no more than a jump, and is
    /// put down without a landing, and the picker redraws on a key and nothing
    /// else, as it does the rest of the time.
    ///
    /// One switch for the flight, its moves and the landing: they are the one
    /// carry, from picking the session up to putting it down.
    ///
    /// Unlike the fade, `NO_COLOR` leaves this alone. The trail, the sparks and
    /// the landing are braille and modifiers, and the sidestep moves whole
    /// cells; the one part in colour is a row letting the bar go, which
    /// without colour holds it as a modifier instead, as the cursor's
    /// afterglow does.
    pub enabled: bool,
}

/// The row the cursor leaves, fading from the selection's reversed bar back
/// to a plain row (see [`crate::ui::effects`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CursorSettings {
    /// This effect's own switch, under `[effects] enabled`.
    ///
    /// It is in colour, so it wants what the fade wants: the terminal's
    /// answer to the colour query, and no `NO_COLOR`. Without them it falls
    /// back to the bar standing a moment longer, reversed and dim, before it
    /// goes — a modifier, like the rest of the picker. How long it lasts is
    /// [`crate::ui::effects::AFTERGLOW`].
    pub enabled: bool,
}

/// An attach from the picker: the picker closing in from the top and the
/// bottom onto the chosen row with the session's name kept on the screen,
/// through the client's start, and the session opening out of that line as
/// the name dissolves into it (see [`crate::handoff`] and
/// [`crate::fade::Iris`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AttachSettings {
    /// This effect's own switch, under `[effects] enabled`. Off, the picker
    /// dissolves whole, as it did.
    ///
    /// It adds no frame of its own, only changes what the fade's frames show,
    /// so it needs the fade on and `[effects.fade] session` with it; with
    /// either off, or under `NO_COLOR`, an attach is what it would have been
    /// without this.
    pub enabled: bool,
}

/// A kill, in its two halves: while the `[y/N]` asks, a line struck through
/// the session's name ([`crate::ui::effects`]); once it is confirmed, the row
/// erased from its end behind a cursor before the kill runs
/// ([`crate::ui::backspace`]). One switch for both, being the one kill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KillSettings {
    /// This effect's own switch, under `[effects] enabled`. Off, the question
    /// leaves the row as it is, and the row stays as it was until the listing
    /// after the kill takes it away.
    ///
    /// `NO_COLOR` leaves this alone: the line is the crossed-out modifier and
    /// the backspace is characters. Only the warmth the struck row's bar takes
    /// on is colour, and that is left out without one.
    pub enabled: bool,
}

/// The rows a filter keystroke drops, fading out where they stood (see
/// [`crate::ui::effects`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FilterSettings {
    /// This effect's own switch, under `[effects] enabled`. On, the rows a
    /// keystroke drops fade out where they stand before the list closes up —
    /// in colour where the terminal said what its colours are, dim where it
    /// did not. Off, they vanish on the keystroke.
    pub enabled: bool,
}

/// `c` on the picker: the rows under the cursor step down a line, and the
/// session about to be made fades into the gap, dim, under the name the prompt
/// will offer and the number it will have — then the picker closes onto that
/// row and the prompt opens out of its name field (see
/// [`crate::ui::app::App::make_room`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CreateSettings {
    /// This effect's own switch, under `[effects] enabled`. Off, the prompt
    /// replaces the picker on the next frame, as it did.
    ///
    /// The gap and the row in it are layout and the dim modifier, so
    /// `NO_COLOR` leaves them alone. The row fading in, and the close and the
    /// opening either side of the prompt, are the fade's: without it the gap
    /// is held a moment, dim, and then the prompt cuts in.
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

// On or off as Neovide has them, where it has the same effect — but for the
// cursor's travel, off unless turned on (`SmearSettings` derives its off).

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

// These are the built-in behaviour. `#[serde(default)]` on the containers means
// an absent file, an empty file and an omitted field all land here, so this is
// the one place a default is written down.

impl Default for KeySettings {
    fn default() -> Self {
        Self {
            // Not a literal: `keys::PREFIX` is read directly by the first-run
            // screen and by `--help`, which are printed before any config is
            // loaded, so it has to exist on its own.
            prefix: crate::keys::PREFIX,
            // A second, where this was half of one. What made the shorter wait
            // right was that a lone `<prefix>` showed nothing: the screen sat
            // unchanged, so every millisecond of the wait was a millisecond the
            // editor looked like it had dropped a keystroke, and the sooner the
            // byte went through as a literal the better. The hint bar
            // ([`crate::hint`]) ended that — the wait is now on screen, saying
            // what it is waiting for — so the number can be what it should have
            // been all along: long enough to read the row and choose from it,
            // rather than short enough to hide that anything was pending.
            //
            // It is not only the prefix's wait. The same value is how long a
            // half-typed session number waits for another digit, in the relay
            // and in the picker ([`crate::ui`]), and both of those show the
            // digits so far while they wait. Every use of it is now visible,
            // which is what makes one number right for all three.
            timeout_ms: 1000,
            // Every command on the key `keys::BINDINGS` gives it, which is
            // where each of those is written down, once.
            bind: crate::keys::Keymap::default(),
        }
    }
}

impl Default for SessionSettings {
    fn default() -> Self {
        Self {
            // Not a literal, for the same reason as the prefix above: the
            // prompt shows this before any config has necessarily been read.
            command: crate::launch::DEFAULT.to_string(),
        }
    }
}

impl FadeSettings {
    /// One direction of a dissolve: half of [`FadeSettings::duration_ms`],
    /// which measures both.
    ///
    /// Halved here rather than at the two schedules that start a fade, so
    /// there is one place where the config's units become the code's and
    /// nowhere that can read the key as a single direction by mistake.
    ///
    /// Exact, and never zero: `Duration` divides at nanosecond resolution, so
    /// the smallest legal setting — 1 ms — halves to 500 µs rather than to
    /// nothing. A fade that short is one frame either way (see
    /// [`crate::fade::Schedule::next`]), which is what any value at or under
    /// a frame has always been.
    pub fn one_way(&self) -> Duration {
        Duration::from_millis(self.duration_ms) / 2
    }
}

impl Default for FadeSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            // Long enough to read as a dissolve rather than a flicker, short
            // enough to be one movement: 100 ms out and 100 ms back.
            duration_ms: 200,
            session: true,
        }
    }
}

/// The prefix comes in as a human string; delegate to the one parser so the file
/// and any other caller agree, and fold its message into serde's error (which
/// TOML then reports with the offending span).
fn de_prefix<'de, D>(deserializer: D) -> Result<u8, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    crate::keys::parse_prefix(&s).map_err(serde::de::Error::custom)
}

/// `[keys.bind]` comes in as a table of command names and human key strings,
/// and goes onto the defaults: a command it leaves out stays on its own key.
/// Which two commands share a key is a question about the whole table, so it is
/// [`Settings::validate`]'s.
impl<'de> Deserialize<'de> for crate::keys::Keymap {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let moved =
            std::collections::BTreeMap::<CommandName, CommandKey>::deserialize(deserializer)?;
        let mut keymap = crate::keys::Keymap::default();
        for (CommandName(name), CommandKey(key)) in moved {
            // A name the table has, or it would not have been read, so this
            // always lands.
            keymap.bind(&name, key);
        }
        Ok(keymap)
    }
}

/// A command's name in `[keys.bind]`: one of the table's own
/// ([`crate::keys::NAMES`]), so a command cannot be added without being one a
/// file can move. Any other is refused in the words `deny_unknown_fields` uses
/// for every other typo in the file, and TOML points at the name.
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

/// The key a command is put on, read by the one parser
/// ([`crate::keys::parse_key`]) and its message folded into serde's error,
/// which TOML then reports with the offending span — the shape `de_prefix`
/// gives the prefix.
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

/// Ceiling for `keys.timeout_ms`. Generous but finite: the value goes into a
/// `poll` timeout as a `c_int`, and an absurd one is a hang, not a long wait.
pub(crate) const MAX_TIMEOUT_MS: u64 = 60_000;

/// Ceiling for `effects.fade.duration_ms`. Two seconds is already a transition nobody
/// wants to sit through, and a switch sits through one at every change of
/// session; past it the value is a hang with a name.
pub(crate) const MAX_FADE_MS: u64 = 2_000;

impl Settings {
    /// Rules that the deserializer cannot express. Kept minimal: only values that
    /// would misbehave at runtime, not taste.
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.keys.timeout_ms == 0 {
            // A zero poll timeout spins the relay at full speed while a prefix
            // is armed, and a number could never be typed.
            return Err("keys.timeout_ms must be at least 1".into());
        }
        if self.keys.timeout_ms > MAX_TIMEOUT_MS {
            // Beyond `c_int` this would wrap negative and `poll` would block
            // forever; well before that it is a prefix that never resolves.
            return Err(format!("keys.timeout_ms must be at most {MAX_TIMEOUT_MS}"));
        }
        if let Some((first, second, key)) = self.keys.bind.clash() {
            // The second could never be reached, and the help and the hint bar
            // would offer it all the same. Both are named, since moving either
            // is the fix — and one may be a command the file never mentions,
            // still on its own key when another was moved onto it.
            return Err(format!(
                "keys.bind.{first} and keys.bind.{second} are both {:?}",
                crate::keys::key_label(key)
            ));
        }
        if self.effects.fade.duration_ms == 0 {
            // `enabled = false` is how the fade is turned off; a zero-length
            // fade would be the same thing spelled as a schedule with no frames.
            // One millisecond still halves to something rather than to nothing,
            // which is what [`FadeSettings::one_way`] is careful about.
            return Err("effects.fade.duration_ms must be at least 1".into());
        }
        if self.effects.fade.duration_ms > MAX_FADE_MS {
            // It feeds a `sleep`, on every screen a switch dissolves.
            return Err(format!(
                "effects.fade.duration_ms must be at most {MAX_FADE_MS}"
            ));
        }
        // Refused at startup rather than at the prompt: a command that can never
        // spawn a session is a broken config, and the file is where it is fixed.
        //
        // The reason alone, not the whole error: every one of them is phrased as
        // a predicate, so it reads as one sentence about the key — the same
        // shape the timeout's messages above have.
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
    /// No path could be formed (no `$HOME`); use the defaults.
    None,
}

/// An empty value is treated as unset, matching how a shell exports a variable
/// that was never really set. Shared with [`crate::state`], which takes the
/// same view of the same kind of variable.
pub(crate) fn nonempty(value: Option<&OsStr>) -> Option<&OsStr> {
    value.filter(|s| !s.is_empty())
}

/// Resolve the config path from the relevant environment, purely.
///
/// Precedence: `$NVMUX_CONFIG` (an exact path) > `$XDG_CONFIG_HOME/nvmux/` >
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

/// Load the configuration, or the defaults when there is no file to load.
///
/// A missing *default* file is not an error; a missing *explicit* file
/// (`$NVMUX_CONFIG`) is. Any file that exists is read and validated, and every
/// failure carries its path.
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

/// The path a first-run config should be written to, or `None` when nvmux must
/// not offer to create one: `$NVMUX_CONFIG` is set (the user owns that file), no
/// default path can be formed (no `$HOME`/`$XDG_CONFIG_HOME`), or a config
/// already exists at the default path.
pub fn first_run_target() -> Option<PathBuf> {
    let nvmux_config = std::env::var_os("NVMUX_CONFIG");
    let xdg = std::env::var_os("XDG_CONFIG_HOME");
    let home = std::env::var_os("HOME");
    match resolve_config_path(nvmux_config.as_deref(), xdg.as_deref(), home.as_deref()) {
        ConfigSource::Default(path) if !path.exists() => Some(path),
        _ => None,
    }
}

/// The settings for a session started from the first-run prompt: the chosen
/// prefix over the built-in defaults.
pub fn with_prefix(prefix: u8) -> Settings {
    Settings {
        keys: KeySettings {
            prefix,
            ..KeySettings::default()
        },
        ..Settings::default()
    }
}

/// The first-run config file body: a commented template that documents every
/// option but activates only `[keys] prefix`. Recording just the choice means
/// the file does not pin the other defaults — they keep tracking the code. The
/// commented values are the current defaults, so the template stays accurate.
///
/// A line each, as the README's list of every setting has them: the long
/// account of an option, what it costs included, is on its field here.
pub(crate) fn render_default_config(prefix: u8) -> String {
    let k = KeySettings::default();
    let s = SessionSettings::default();
    let c = ClientSettings::default();
    let t = ThemeSettings::default();
    let e = EffectsSettings::default();
    let f = e.fade;
    // A string as the file spells it. Quoted here rather than with `{:?}` in
    // the template, which would ignore the widths that line up a table's
    // notes: each setting is padded to the widest in its table, and the
    // prefix to the widest it can be, "Ctrl-Space".
    let quoted = |s: &str| format!("{s:?}");
    // A line a command, made from the table rather than written out here, so a
    // command cannot be added without one: its name, its key as `key_label`
    // spells it — which `parse_key` reads back — and what the help screen says
    // it does.
    let bind: String = crate::keys::BINDINGS
        .iter()
        .map(|b| {
            format!(
                "# {:<6} = {:<7}   # {}\n",
                b.hint,
                quoted(&crate::keys::key_label(b.key)),
                b.help
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
         # ui           = {ui:<8}   # nvmux's own animated client, or \"nvim\" for Neovim's\n\
         # per_session  = {per_session:<8}   # keep each session's client; a switch back reuses it\n\
         # lazy         = {lazy:<8}   # start every client at launch; true waits for each session's first visit\n\
         # predict      = {predict:<8}   # over a slow link, scrolls show before Neovim answers\n\
         # centre       = {centre:<8}   # on a wide terminal, each window's text in the middle of the window\n\
         # window_width = {window_width:<8}   # with centre on, how many columns a window's text gets; 0 for no limit\n\
         # smart_width  = {smart_width:<8}   # or as many as what it shows wants: its 'textwidth', its long lines\n\
         \n\
         [theme]\n\
         # color = {theme_color}   # or \"#rrggbb\": nvmux's text and bars in that colour\n\
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
        predict = c.predict,
        centre = c.centre,
        window_width = c.window_width,
        smart_width = c.smart_width,
        theme_color = quoted(&t.color.name()),
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

/// Write the first-run config to `path`, creating its parent directory.
///
/// Atomic (temp file plus `rename` within the directory, see
/// [`crate::paths::write_atomic`]), under a parent made the gentle way rather
/// than the `/tmp`-hardened one — see [`crate::paths::create_private_parent`]
/// for why a pre-existing `~/.config` must not be refused.
pub fn write_default(path: &Path, prefix: u8) -> Result<(), ConfigError> {
    let err = |error| ConfigError::Write {
        path: path.to_path_buf(),
        error,
    };
    crate::paths::create_private_parent(path).map_err(err)?;
    crate::paths::write_atomic(path, render_default_config(prefix).as_bytes()).map_err(err)
}

/// The settings a process runs on, and what its file is known to hold.
///
/// One of these is the process-global behind [`get`]; tests make their own,
/// so that a change one of them applies is never what another reads.
struct Store {
    /// What [`get`] hands out: the file's settings as loaded, and whatever has
    /// been changed since.
    live: RwLock<Arc<Settings>>,
    /// What the file holds, as far as this process knows: as loaded, and then
    /// as last saved. What differs from `live` is unsaved.
    saved: RwLock<Arc<Settings>>,
    /// How many changes `live` has seen. See [`generation`].
    generation: AtomicU64,
    /// Whether [`Store::init`] has run.
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

/// A lock that a panic elsewhere left poisoned still holds whole settings: a
/// value is only ever swapped in whole, never edited in place.
fn read(lock: &RwLock<Arc<Settings>>) -> std::sync::RwLockReadGuard<'_, Arc<Settings>> {
    lock.read().unwrap_or_else(|e| e.into_inner())
}

fn write(lock: &RwLock<Arc<Settings>>) -> std::sync::RwLockWriteGuard<'_, Arc<Settings>> {
    lock.write().unwrap_or_else(|e| e.into_inner())
}

static STORE: LazyLock<Store> = LazyLock::new(Store::new);

/// Install the loaded settings, once, before anything reads them. `main` calls
/// this at startup, and the relay tests' child process does the same; a second
/// call in one process is a bug, so it warns and keeps the first.
///
/// They are what the file holds, too, as far as nvmux knows: see [`saved`].
pub fn init(settings: Settings) {
    STORE.init(settings);
}

/// The settings nvmux is running on: those loaded, as the settings screen has
/// changed them since. The defaults when [`init`] has not run — which is what
/// keeps unit tests reading the compiled defaults, never a real user's file.
///
/// Asked for each time a value is needed rather than kept, so that a change
/// reaches its reader: the `Arc` is a snapshot, as current as the moment it
/// was taken.
pub fn get() -> Arc<Settings> {
    STORE.get()
}

/// Run on `settings` from now on, if they are valid — the rules [`load`]
/// holds a file to — and count the change (see [`generation`]). Applying the
/// settings already running changes nothing and counts nothing.
pub fn apply(settings: Settings) -> Result<(), String> {
    STORE.apply(settings)
}

/// How many changes [`apply`] has made since nvmux started. A reader that
/// copied what it needed — a client told its settings, the picker its
/// effects — notes the generation it copied at, and knows its copy is stale
/// when this has moved on.
pub fn generation() -> u64 {
    STORE.generation.load(Ordering::SeqCst)
}

/// What the file holds, as far as nvmux knows: what [`init`] was given, until
/// a save says otherwise ([`set_saved`]). Whatever [`get`] differs from it in
/// is unsaved.
pub fn saved() -> Arc<Settings> {
    STORE.saved()
}

/// Record that the file now holds `settings`, once a save has written them.
pub fn set_saved(settings: Settings) {
    STORE.set_saved(settings);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // --- defaults / parsing (pure) -----------------------------------------

    /// The one default that is not written here: `keys::PREFIX` exists on its
    /// own because `--help` and the first-run screen are printed before any
    /// config is read. The two must agree.
    ///
    /// Every other default is pinned by
    /// `a_full_document_at_the_defaults_round_trips`, which spells each value
    /// out in TOML and asserts the result equals `Settings::default()`.
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

    /// The README's "Every setting, at its default" is a copy of these
    /// defaults that nothing else keeps in step: it must parse, and to them,
    /// with every table in it that the code has.
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
            "[theme]",
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
            predict = true\n\
            [theme]\n\
            color = \"terminal\"\n\
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

    /// The key measures a whole dissolve, so one direction is half of it.
    ///
    /// The floor is the case worth pinning: `duration_ms = 1` is legal, and
    /// halving it in whole milliseconds would be zero — a schedule with no
    /// length, which is the very thing `validate` refuses to let the key
    /// express.
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

    /// One switch over all of them: off, no effect runs, and each one's own
    /// switch is left as it was for when the master is turned back on.
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

    /// The picker's effects each turn off on their own.
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

    /// Each effect has a switch of its own, and turning one off leaves the
    /// others running.
    #[test]
    fn the_move_effect_is_on_unless_turned_off() {
        assert!(Settings::default().effects.moving.enabled);
        let s: Settings = toml::from_str("[effects.move]\nenabled = false\n").expect("valid");
        assert!(!s.effects.move_enabled());
        assert!(s.effects.fade_enabled(), "and the fade is untouched");
    }

    /// On unless turned off, and `false` turns it off: a parked client is a
    /// UI its session's other users share a grid with, and `false` is how
    /// whoever runs this nvmux spares them that.
    #[test]
    fn one_client_per_session_is_on_unless_turned_off() {
        assert!(Settings::default().client.per_session);
        let s: Settings = toml::from_str("[client]\nper_session = false\n").expect("valid");
        assert!(!s.client.per_session);
        assert_eq!(s.effects, EffectsSettings::default());
        assert_eq!(s.session, SessionSettings::default());
    }

    /// Started at launch unless made lazy, and making them lazy leaves
    /// `per_session` as it was: the two are independent, and either can be
    /// set alone.
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

    /// Scrolls are predicted over a slow link unless turned off.
    #[test]
    fn scrolls_are_predicted_unless_turned_off() {
        assert!(Settings::default().client.predict);
        let s: Settings = toml::from_str("[client]\npredict = false\n").expect("valid");
        assert!(!s.client.predict);
        assert!(s.client.per_session && !s.client.lazy, "the rest as it was");
    }

    /// A wide terminal is left alone unless told otherwise, and each window's
    /// text centred up to 120 columns when it is: a switch, on or off, and
    /// anything else an error.
    #[test]
    fn nothing_is_centred_unless_told() {
        let c = Settings::default().client;
        assert_eq!((c.centre, c.window_width), (false, 120));
        for on in [false, true] {
            let s: Settings = toml::from_str(&format!("[client]\ncentre = {on}\n")).expect("valid");
            assert_eq!(s.client.centre, on);
            assert_eq!(s.client.window_width, 120, "the rest as it was");
        }
        let s: Settings = toml::from_str("[client]\nwindow_width = 0\n").expect("valid");
        assert_eq!(s.client.window_width, 0);
        assert!(toml::from_str::<Settings>("[client]\ncentre = \"windows\"\n").is_err());
        assert!(toml::from_str::<Settings>("[client]\nwindow_width = -1\n").is_err());
    }

    /// Each window's text as wide as what it shows wants unless told
    /// otherwise; `window_width` for every one of them is a line away.
    #[test]
    fn widths_are_judged_unless_told() {
        assert!(Settings::default().client.smart_width);
        let s: Settings = toml::from_str("[client]\nsmart_width = false\n").expect("valid");
        assert!(!s.client.smart_width);
        assert_eq!(s.client.window_width, 120, "the rest as it was");
    }

    /// nvmux's own client unless told otherwise; Neovim's is a line away.
    #[test]
    fn a_session_is_drawn_by_nvmuxs_client_unless_told() {
        assert_eq!(Settings::default().client.ui, Ui::Nvmux);
        let s: Settings = toml::from_str("[client]\nui = \"nvim\"\n").expect("valid");
        assert_eq!(s.client.ui, Ui::Nvim);
        assert!(s.client.per_session && !s.client.lazy, "the rest as it was");
    }

    /// The terminal's own colours unless told otherwise; a colour is a line
    /// away, in either of the two spellings a person writes one in, and is
    /// spelt back the long way.
    #[test]
    fn the_screens_are_in_the_terminals_colours_unless_told() {
        assert_eq!(Settings::default().theme.color, ThemeColor::Terminal);
        assert_eq!(ThemeColor::Terminal.name(), "terminal");
        for (spelt, rgb, name) in [
            ("#7aa2f7", Rgb(0x7a, 0xa2, 0xf7), "#7aa2f7"),
            ("#7AA2F7", Rgb(0x7a, 0xa2, 0xf7), "#7aa2f7"),
            ("#f80", Rgb(0xff, 0x88, 0x00), "#ff8800"),
            ("#000", Rgb(0, 0, 0), "#000000"),
        ] {
            let s: Settings =
                toml::from_str(&format!("[theme]\ncolor = \"{spelt}\"\n")).expect(spelt);
            assert_eq!(s.theme.color, ThemeColor::Mono(rgb), "{spelt}");
            assert_eq!(s.theme.color.name(), name, "{spelt}");
            assert_eq!(
                ThemeColor::parse(&s.theme.color.name()),
                Ok(s.theme.color),
                "{spelt} round trips"
            );
            assert_eq!(s.effects, EffectsSettings::default(), "the rest as it was");
        }
        let s: Settings = toml::from_str("[theme]\ncolor = \"terminal\"\n").expect("valid");
        assert_eq!(s, Settings::default(), "the default, said out loud");
    }

    /// A colour that is not one is refused with the key it was in and the
    /// spellings that would have done.
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

    /// On and off as Neovide's are: the scroll and the windows on, the
    /// particles and the smooth blink off — but for the cursor's travel, off
    /// unless turned on.
    #[test]
    fn the_clients_effects_start_as_neovides() {
        let e = Settings::default().effects;
        assert!(e.scroll_enabled() && e.windows_enabled());
        assert!(!e.smear_enabled() && !e.particles_enabled() && !e.blink_enabled());
        assert_eq!(e.particles.mode, ParticleMode::Railgun);
    }

    /// A particle mode is spelt as Neovide spells it.
    #[test]
    fn a_particle_mode_is_spelt_as_neovides() {
        let s: Settings =
            toml::from_str("[effects.particles]\nmode = \"pixiedust\"\n").expect("valid");
        assert_eq!(s.effects.particles.mode, ParticleMode::Pixiedust);
        assert_eq!(ParticleMode::Pixiedust.name(), "pixiedust");
    }

    /// The one default that lives elsewhere, like the prefix: the prompt shows
    /// it before a config has necessarily been read.
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

    /// `[keys.bind]` moves what it names and nothing else: every other command
    /// stays on its own key, and the rest of `[keys]` at its default — in the
    /// table's own form and in TOML's inline one.
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

    /// Two commands on one key would leave one of them unreachable, so the file
    /// is refused, naming both — the one moved, and the one it was moved onto
    /// that the file never mentions. Trading two keys is no clash.
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

    /// A name that is no command is refused in the words every other unknown
    /// key is, offering the names there are; a key that cannot be one says
    /// what would do. Each points at its own line.
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

    /// A config file is edited by hand, so a typo is likely and silence is the
    /// wrong response — an unknown table, an unknown key in any table, and an
    /// unparseable value all have to be refused rather than ignored.
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
            ("an unknown theme key", "[theme]\ncolour = \"#7aa2f7\"\n"),
            ("a colour by name", "[theme]\ncolor = \"blue\"\n"),
            ("a colour with no #", "[theme]\ncolor = \"7aa2f7\"\n"),
            ("a colour as a number", "[theme]\ncolor = 0x7aa2f7\n"),
            (
                "a colour of the wrong length",
                "[theme]\ncolor = \"#7aa2f\"\n",
            ),
            (
                "a theme's table outside [theme]",
                "[color]\ncolor = \"#7aa2f7\"\n",
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
                "an unparseable predict value",
                "[client]\npredict = \"yes\"\n",
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

    /// The timeout feeds a `poll` call and the fade's duration a `sleep`, so
    /// each has a floor and a ceiling; the extreme values that pass are also
    /// pinned so the bounds stay generous.
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

    /// A command that could never spawn a session is a broken config, not a
    /// surprise at the prompt — and the message has to name the key it is in.
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

    /// `main` prints a failed load through anyhow's `{:#}`, which follows the
    /// message with every `source()`. The TOML error is in the message, so it
    /// must not be one of those as well, or it is printed twice.
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
        // As is every command's key, under a table of their own: a line each,
        // in the table's order.
        assert!(rendered.contains("\n[keys.bind]   # "), "{rendered:?}");
        let mut rest = rendered.as_str();
        for b in crate::keys::BINDINGS {
            let line = format!("\n# {:<6} = {:?}", b.hint, crate::keys::key_label(b.key));
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
            "[theme]",
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
            "# per_session  = true",
            "# lazy         = false",
            "# ui           = \"nvmux\"",
            "# predict      = true",
            "# centre       = false",
            "# window_width = 120",
            "# smart_width  = true",
            "# color = \"terminal\"",
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

    /// The file is edited by uncommenting a line, its note and all, so each
    /// commented setting must parse once uncommented, and to its default; and
    /// between them they must be every setting the README lists, spelt as it
    /// spells them — which is what keeps a line from going missing.
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

    // --- live settings (each test its own store) ---------------------------

    fn generation_of(store: &Store) -> u64 {
        store.generation.load(Ordering::SeqCst)
    }

    /// A change is what `get` hands out from then on, and counts one
    /// generation; the file is as it was until a save says otherwise.
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

    /// What a file would be refused for, a change is refused for, with the
    /// same words, and nothing changes.
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

    /// The settings loaded are both what runs and what the file holds, and a
    /// second load in one process is a mistake that changes neither.
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
    /// process-global. No other test in the crate touches them, so this guard
    /// only serialises these tests against each other.
    static ENV_GUARD: Mutex<()> = Mutex::new(());

    /// Unset the three vars for the duration of a test, restoring them on drop
    /// so nothing leaks between tests even on a panic.
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
