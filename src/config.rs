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

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use serde::Deserialize;

use crate::error::ConfigError;

/// The whole configuration. Each field is a table of its own, so the file reads
/// as `[keys]`-style sections — `[effects]` with a table inside it for each
/// effect: `[effects.fade]`, `[effects.move]`, `[effects.cursor]`,
/// `[effects.back]`, `[effects.attach]`, `[effects.kill]`,
/// `[effects.filter]` and `[effects.create]` for nvmux's own screens, and
/// `[effects.smear]`, `[effects.particles]`, `[effects.scroll]`,
/// `[effects.windows]` and `[effects.blink]` for nvmux's own client
/// (`[client] ui = "nvmux"`, see [`crate::client`]).
///
/// Not `Copy`: `[session] command` owns a `String`. Nothing reads it by value —
/// [`get`] hands out a `&'static Settings` — so this costs nothing.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub keys: KeySettings,
    pub session: SessionSettings,
    pub client: ClientSettings,
    pub effects: EffectsSettings,
}

/// The prefix key and how long a half-typed sequence waits (see [`crate::keys`]).
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
/// [`crate::pty`]): Neovim's own, `nvim --remote-ui`, unless `ui` says
/// nvmux's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClientSettings {
    /// Which client draws a session.
    ///
    /// `"nvim"`, the default, is Neovim's own TUI: everything the terminal
    /// can do reaches the editor exactly as Neovim negotiated it, because
    /// Neovim did the negotiating. `"nvmux"` is nvmux's own client
    /// ([`crate::client`]), which draws the editor itself and so can animate
    /// it as Neovide does — the cursor travelling, the scroll, windows moving
    /// — under `[effects.smear]`, `[effects.particles]`, `[effects.scroll]`,
    /// `[effects.windows]` and `[effects.blink]`.
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
    /// On, the default, a session has no client until it is first attached
    /// to — a fork, a probe, and a first paint to wait for, as on a switch to
    /// it with `per_session` off. Off, nvmux starts one for every session
    /// there is before the picker goes up, and holds each out of sight until
    /// its session is first visited (see
    /// [`crate::pty::Attachment::start_ahead`]). That visit then waits for
    /// nothing: what the client drew meanwhile goes onto the terminal as it
    /// was written, its questions to the terminal with it, and the terminal's
    /// answers come back to it then. A session created after nvmux started is
    /// started on its first visit either way.
    ///
    /// What off costs:
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
}

impl Default for ClientSettings {
    fn default() -> Self {
        Self {
            ui: Ui::Nvim,
            per_session: true,
            lazy: true,
        }
    }
}

/// `[client] ui`: whose client draws a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Ui {
    /// `nvim --remote-ui`: Neovim's own TUI.
    #[default]
    Nvim,
    /// `nvmux --client`: nvmux's own, animated (see [`crate::client`]).
    Nvmux,
}

/// The animated effects: a master switch over them all, and a table of its own
/// for each — `[effects.fade]`, `[effects.move]`, `[effects.cursor]`,
/// `[effects.back]`, `[effects.attach]`, `[effects.kill]`,
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
    pub back: BackSettings,
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

    /// Whether the cursor lights the rows it moves between — the row it leaves
    /// glows, and a glint crosses the one it lands on: its own switch and the
    /// master one. Whether they are drawn in colour or with a modifier is the
    /// terminal's say after this (see [`crate::ui::effects`]).
    pub fn cursor_enabled(&self) -> bool {
        self.enabled && self.cursor.enabled
    }

    /// Whether coming back to the picker from a session sends rings pulsing
    /// out from its row: its own switch and the master one.
    pub fn back_enabled(&self) -> bool {
        self.enabled && self.back.enabled
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
    /// [`FadeSettings::one_way`]). So a switch pays it once for the screens,
    /// and once more for the box that says where it landed, which dissolves in
    /// and out around the second it is up (see [`crate::announce`]).
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
    /// That parse is also what the attach notice dissolves into, so off, the
    /// notice's box empties the cells it covers and waits for a repaint to
    /// fill them (see [`crate::announce`]).
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

/// The rows the cursor moves between: the one it leaves fading from the
/// selection's reversed bar back to a plain row, and the one it lands on
/// crossed by a glint (see [`crate::ui::effects`]). One switch for both,
/// being the two ends of the one move.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CursorSettings {
    /// This effect's own switch, under `[effects] enabled`.
    ///
    /// Both are in colour, so they want what the fade wants: the terminal's
    /// answer to the colour query, and no `NO_COLOR`. Without them the fade
    /// back falls back to the bar standing a moment longer, reversed and dim,
    /// before it goes, and the glint to an underline running under the bar —
    /// modifiers, like the rest of the picker. How long each lasts is
    /// [`crate::ui::effects::AFTERGLOW`] and [`crate::ui::effects::GLINT`].
    pub enabled: bool,
}

/// Coming back to the picker from a session with `<prefix> Space`: rings of
/// braille pulsing out from that session's row every few seconds while the
/// picker is up, so it is plain which one `Esc` returns to (see
/// [`crate::ui::sonar`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BackSettings {
    /// This effect's own switch, under `[effects] enabled`. Off, the picker
    /// comes back up with the cursor on that session and nothing else.
    ///
    /// The rings fade in colour where the terminal said what its colours are;
    /// otherwise, and under `NO_COLOR`, they step down to dim — braille and a
    /// modifier, like the trail.
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
            back: BackSettings::default(),
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

impl Default for BackSettings {
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

/// Ceiling for `keys.timeout_ms`. Generous but finite: the value goes into a
/// `poll` timeout as a `c_int`, and an absurd one is a hang, not a long wait.
const MAX_TIMEOUT_MS: u64 = 60_000;

/// Ceiling for `effects.fade.duration_ms`. Two seconds is already a transition nobody
/// wants to sit through, and a switch sits through two of them — the screens
/// and the notice; past it the value is a hang with a name.
const MAX_FADE_MS: u64 = 2_000;

impl Settings {
    /// Rules that the deserializer cannot express. Kept minimal: only values that
    /// would misbehave at runtime, not taste.
    fn validate(&self) -> Result<(), String> {
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
            Err(source) => Err(ConfigError::Read { path, source }),
        },
        ConfigSource::Explicit(path) => match std::fs::read_to_string(&path) {
            Ok(contents) => parse(&path, &contents),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(ConfigError::ExplicitMissing(path))
            }
            Err(source) => Err(ConfigError::Read { path, source }),
        },
    }
}

/// Parse and validate the file contents, tagging every error with its path.
fn parse(path: &Path, contents: &str) -> Result<Settings, ConfigError> {
    let settings: Settings = toml::from_str(contents).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
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
fn render_default_config(prefix: u8) -> String {
    let k = KeySettings::default();
    let s = SessionSettings::default();
    let c = ClientSettings::default();
    let e = EffectsSettings::default();
    let f = e.fade;
    format!(
        "# nvmux configuration — created on first run.\n\
         #\n\
         # Uncomment and edit any line to override its default; delete this file\n\
         # to start over. See the README for what each option does.\n\
         \n\
         [keys]\n\
         prefix     = {prefix:?}\n\
         # timeout_ms = {timeout}\n\
         \n\
         [session]\n\
         # {{sock}} becomes the session's socket; it is what nvmux finds it by.\n\
         # command = {command:?}\n\
         \n\
         [client]\n\
         # Keep each session's Neovim client while another is in front, so a\n\
         # switch back puts its screen back at once. Until nvmux exits, it costs:\n\
         #  - a kept client is still a UI of its session, so another UI on it,\n\
         #    a second nvmux included, is held to the smaller of the two sizes;\n\
         #  - a switch no longer fires UILeave or UIEnter;\n\
         #  - every session visited keeps an idle client (about 1.5 MB), its ssh\n\
         #    forward, and the redraws its server still sends it.\n\
         # false starts a new client on every switch instead.\n\
         # per_session = {per_session}\n\
         # Start each session's client on its first visit. false starts one for\n\
         # every session as nvmux starts, so a first visit waits for nothing,\n\
         # at the cost above for every session rather than every one visited.\n\
         # lazy = {lazy}\n\
         # Which client draws a session: \"nvim\" is Neovim's own, and everything\n\
         # the terminal can do reaches the editor as Neovim negotiated it;\n\
         # \"nvmux\" is nvmux's own, which animates the editor as Neovide does —\n\
         # the [effects.smear] tables and those after it below.\n\
         # ui = {ui:?}\n\
         \n\
         [effects]\n\
         # The master switch: false turns every effect below off, whatever its\n\
         # own enabled says.\n\
         # enabled = {effects_enabled}\n\
         \n\
         [effects.fade]\n\
         # The dissolve between screens, and of the notice a switch puts up.\n\
         # NO_COLOR turns it off whatever this says.\n\
         # enabled     = {fade_enabled}\n\
         # duration_ms = {fade_duration}\n\
         # session     = {fade_session}\n\
         \n\
         [effects.move]\n\
         # Stars streaming off both ends of a session picked up to be moved,\n\
         # the rows it passes stepping aside, and the impact when it is put\n\
         # down.\n\
         # enabled = {move_enabled}\n\
         \n\
         [effects.cursor]\n\
         # The row the cursor leaves fades back from the selection's bar,\n\
         # and a glint crosses the row it lands on.\n\
         # enabled = {cursor_enabled}\n\
         \n\
         [effects.back]\n\
         # Back in the picker from a session, rings pulse from its row every\n\
         # few seconds while the picker is up.\n\
         # enabled = {back_enabled}\n\
         \n\
         [effects.attach]\n\
         # An attach from the picker closes onto the chosen row, keeping the\n\
         # session's name on screen, and opens the session out of that line.\n\
         # Rides the fade, so it needs [effects.fade] on, with session.\n\
         # enabled = {attach_enabled}\n\
         \n\
         [effects.kill]\n\
         # A line through the name while the [y/N] asks; once it is confirmed,\n\
         # the row is erased from its end before the kill runs.\n\
         # enabled = {kill_enabled}\n\
         \n\
         [effects.filter]\n\
         # Rows a filter keystroke drops fade out before the list closes up.\n\
         # enabled = {filter_enabled}\n\
         \n\
         [effects.create]\n\
         # c on the picker opens a gap where the new session will go, showing\n\
         # the name the prompt will offer, before the prompt comes up.\n\
         # enabled = {create_enabled}\n\
         \n\
         [effects.smear]\n\
         # nvmux's own client ([client] ui = \"nvmux\"): the cursor travels\n\
         # between cells rather than jumping, a long move smearing behind it.\n\
         # enabled = {smear_enabled}\n\
         \n\
         [effects.particles]\n\
         # nvmux's own client: particles fly off the cursor as it moves. mode is\n\
         # railgun, torpedo, pixiedust, sonicboom, ripple or wireframe.\n\
         # enabled = {particles_enabled}\n\
         # mode    = {particles_mode:?}\n\
         \n\
         [effects.scroll]\n\
         # nvmux's own client: a scroll slides the text through the window.\n\
         # enabled = {scroll_enabled}\n\
         \n\
         [effects.windows]\n\
         # nvmux's own client: a split window opening, closing or changing size\n\
         # moves there, and one showing another buffer fades from one to the\n\
         # other, the way animate.nvim draws them; a float, the message area or\n\
         # windows rearranged slide there, as in Neovide.\n\
         # enabled = {windows_enabled}\n\
         \n\
         [effects.blink]\n\
         # nvmux's own client: a blinking block cursor fades out and back in.\n\
         # enabled = {blink_enabled}\n",
        prefix = crate::keys::prefix_label(prefix),
        timeout = k.timeout_ms,
        command = s.command,
        per_session = c.per_session,
        lazy = c.lazy,
        fade_enabled = f.enabled,
        fade_duration = f.duration_ms,
        fade_session = f.session,
        effects_enabled = e.enabled,
        move_enabled = e.moving.enabled,
        cursor_enabled = e.cursor.enabled,
        back_enabled = e.back.enabled,
        attach_enabled = e.attach.enabled,
        kill_enabled = e.kill.enabled,
        filter_enabled = e.filter.enabled,
        create_enabled = e.create.enabled,
        ui = match c.ui {
            Ui::Nvim => "nvim",
            Ui::Nvmux => "nvmux",
        },
        smear_enabled = e.smear.enabled,
        particles_enabled = e.particles.enabled,
        particles_mode = e.particles.mode.name(),
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
    let err = |source| ConfigError::Write {
        path: path.to_path_buf(),
        source,
    };
    crate::paths::create_private_parent(path).map_err(err)?;
    crate::paths::write_atomic(path, render_default_config(prefix).as_bytes()).map_err(err)
}

static SETTINGS: OnceLock<Settings> = OnceLock::new();

/// Install the loaded settings, once, before anything reads them. `main` calls
/// this at startup, and the relay tests' child process does the same; a second
/// call in one process is a bug, so it warns and keeps the first.
pub fn init(settings: Settings) {
    if SETTINGS.set(settings).is_err() {
        tracing::warn!("settings initialised more than once; keeping the first");
    }
}

/// The active settings. Falls back to the defaults when [`init`] has not run —
/// which is what keeps unit tests reading the compiled defaults, never a real
/// user's file.
pub fn get() -> &'static Settings {
    SETTINGS.get_or_init(Settings::default)
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

    /// The README's "Every setting, at its default" is a copy of these
    /// defaults that nothing else keeps in step: it must parse, and to them,
    /// with every table in it that the code has.
    #[test]
    fn the_readmes_settings_are_the_defaults() {
        let readme = include_str!("../README.md");
        let start = readme
            .find("<summary>Every setting, at its default</summary>")
            .expect("the README's settings block");
        let block = &readme[start..];
        let open = block.find("```toml\n").expect("its TOML") + "```toml\n".len();
        let close = open + block[open..].find("```").expect("its end");
        let doc = &block[open..close];
        let s: Settings = toml::from_str(doc).expect("the README's settings parse");
        assert_eq!(
            s,
            Settings::default(),
            "the README's settings are not the defaults"
        );
        for table in [
            "[keys]",
            "[session]",
            "[client]",
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
            [session]\n\
            command = \"nvim --headless --listen {sock}\"\n\
            [client]\n\
            ui = \"nvim\"\n\
            per_session = true\n\
            lazy = true\n\
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
            [effects.back]\n\
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
        assert!(!s.effects.back_enabled());
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
        assert!(s.effects.cursor.enabled && s.effects.back.enabled);
        assert!(s.effects.attach.enabled);
        assert!(s.effects.kill.enabled && s.effects.filter.enabled);
        assert!(s.effects.create.enabled);
        let on = Settings::default().effects;
        assert!(on.fade_enabled() && on.move_enabled() && on.cursor_enabled());
        assert!(on.back_enabled() && on.attach_enabled());
        assert!(on.kill_enabled() && on.filter_enabled());
        assert!(on.create_enabled());
    }

    /// The picker's effects each turn off on their own.
    #[test]
    fn each_picker_effect_turns_off_on_its_own() {
        let s: Settings = toml::from_str("[effects.cursor]\nenabled = false\n").expect("valid");
        assert!(!s.effects.cursor_enabled());
        assert!(s.effects.back_enabled() && s.effects.kill_enabled());
        assert!(s.effects.filter_enabled());

        let s: Settings = toml::from_str("[effects.back]\nenabled = false\n").expect("valid");
        assert!(!s.effects.back_enabled());
        assert!(s.effects.cursor_enabled() && s.effects.kill_enabled());
        assert!(s.effects.filter_enabled());

        let s: Settings = toml::from_str("[effects.attach]\nenabled = false\n").expect("valid");
        assert!(!s.effects.attach_enabled());
        assert!(s.effects.back_enabled() && s.effects.kill_enabled());
        assert!(s.effects.fade_enabled(), "the fade itself is untouched");

        let s: Settings = toml::from_str("[effects.kill]\nenabled = false\n").expect("valid");
        assert!(!s.effects.kill_enabled());
        assert!(s.effects.cursor_enabled() && s.effects.back_enabled());
        assert!(s.effects.filter_enabled());

        let s: Settings = toml::from_str("[effects.filter]\nenabled = false\n").expect("valid");
        assert!(!s.effects.filter_enabled());
        assert!(s.effects.cursor_enabled() && s.effects.back_enabled());
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

    /// Lazy unless turned off, and turning it off leaves `per_session` as it
    /// was: the two are independent, and either can be set alone.
    #[test]
    fn clients_start_on_their_first_visit_unless_turned_off() {
        assert!(Settings::default().client.lazy);
        let s: Settings = toml::from_str("[client]\nlazy = false\n").expect("valid");
        assert!(!s.client.lazy);
        assert!(s.client.per_session, "and the clients are still kept");
        let s: Settings =
            toml::from_str("[client]\nper_session = false\nlazy = false\n").expect("valid");
        assert!(!s.client.lazy && !s.client.per_session);
    }

    /// Neovim's own client unless told otherwise: nvmux's is the choice to
    /// make, not the one made for you.
    #[test]
    fn a_session_is_drawn_by_neovims_client_unless_told() {
        assert_eq!(Settings::default().client.ui, Ui::Nvim);
        let s: Settings = toml::from_str("[client]\nui = \"nvmux\"\n").expect("valid");
        assert_eq!(s.client.ui, Ui::Nvmux);
        assert!(s.client.per_session && s.client.lazy, "the rest as it was");
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

    /// A config file is edited by hand, so a typo is likely and silence is the
    /// wrong response — an unknown table, an unknown key in any table, and an
    /// unparseable value all have to be refused rather than ignored.
    #[test]
    fn a_typo_is_never_silently_ignored() {
        for (what, doc) in [
            ("an unknown top-level table", "[colours]\nx = 1\n"),
            ("an unknown keys key", "[keys]\nprefx = \"C-a\"\n"),
            ("an unparseable prefix", "[keys]\nprefix = \"nope\"\n"),
            ("an unknown session key", "[session]\ncmd = \"nvim\"\n"),
            ("an unknown fade key", "[effects.fade]\nduration = 40\n"),
            ("an unknown client key", "[client]\nkeep = true\n"),
            ("an unknown effect", "[effects.sparkles]\nenabled = true\n"),
            (
                "an unknown cursor key",
                "[effects.cursor]\nenable = false\n",
            ),
            ("an unknown back key", "[effects.back]\nenable = false\n"),
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
            rendered.contains("\nprefix     = \"Ctrl-a\"\n"),
            "the chosen prefix is the one active setting: {rendered:?}"
        );
        // The timeout, the command and the fade are documentation, not active
        // settings.
        assert!(rendered.contains("# timeout_ms = 1000"));
        assert!(
            rendered.contains("# command = \"nvim --headless --listen {sock}\""),
            "the template must document the command: {rendered:?}"
        );
        assert!(rendered.contains("\n[client]\n"), "{rendered:?}");
        for table in [
            "[effects]",
            "[effects.fade]",
            "[effects.move]",
            "[effects.cursor]",
            "[effects.back]",
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
            rendered.matches("\n# enabled = true\n").count(),
            10,
            "the master switch and each switch-only effect on by default: {rendered:?}"
        );
        assert_eq!(
            rendered.matches("\n# enabled = false\n").count(),
            3,
            "the cursor's travel, the particles and the smooth blink: {rendered:?}"
        );
        for line in [
            "# per_session = true",
            "# lazy = true",
            "# ui = \"nvim\"",
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
