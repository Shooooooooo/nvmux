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

use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, SaveError};
use crate::palette::Rgb;

/// The whole configuration. Each field is a table of its own, so the file reads
/// as `[keys]`-style sections — `[keys.bind]` inside `[keys]` for the key after
/// the prefix that runs each command, `[style]` for how nvmux's own screens
/// look — the colour they are drawn in, and whether they float over a session
/// — and `[effects]` with a table inside it for each
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
    pub style: StyleSettings,
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
    /// Once a switch of its own, whether a listing waits a quarter of a
    /// second on a session that cannot answer, rather than three: always so
    /// now (see [`crate::rpc::QUICK_PROBE_TIMEOUT`]). Still read, so that a
    /// file that sets it loads, and never looked at.
    pub quick_list: Retired,
    /// Once a switch of its own, whether the sessions say when they have news
    /// rather than are asked for it every two seconds: always so now (see
    /// [`crate::tell`]). Still read, so that a file that sets it loads, and
    /// never looked at.
    pub push_news: Retired,
}

/// The client that draws a session, on the pty nvmux relays (see
/// [`crate::pty`]): nvmux's own, `nvmux --client`, unless `ui` says
/// Neovim's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    /// either way, unless a change of `ui` comes first.
    ///
    /// Clients are started again the same way. Each client kept for a session
    /// not in front, parked or started ahead, rides the link to its session,
    /// and leaves when the link does: over SSH, where there is a link to
    /// drop, the session in front is attached again as the link comes back.
    /// Off, the clients the link took are then started ahead again on a
    /// thread of their own, once the session in front has painted, and the
    /// next visit to each finds its client waiting. On, each is attached
    /// afresh on its next visit: a fork, a forward, a probe and a first
    /// paint, the last two each a round trip or more over the link that has
    /// only just come back. A change of `ui` on the settings screen retires
    /// every client there is, for the next attach to start the new kind. Off,
    /// the client of every other session in the listing is then started as
    /// the new kind, behind that one once it has painted; on, each is started
    /// on its next visit.
    ///
    /// What off costs, and so the reasons to turn it on:
    ///
    /// - Every session has a client — a UI of its server, with all that
    ///   `per_session` says that costs — from the moment nvmux starts, not
    ///   from its first visit.
    /// - nvmux's start pays a listing and, per session, a fork and an ssh
    ///   forward, before the picker is drawn.
    /// - Every session is probed as nvmux starts, and again after a change of
    ///   `ui`, which ends a hit-enter prompt in each, as a visit would (see
    ///   `pty::probe_on`).
    /// - A client that writes more than [`crate::pty::AHEAD_MAX`] before its
    ///   first visit — a `:terminal` running something — is let go, and its
    ///   session is started on its first visit after all.
    /// - In the moments after a dropped link comes back, a fork and an ssh
    ///   forward per session it took a client of, and each one's screen sent
    ///   over the link once more, while the session in front is in use. On a
    ///   slow link its next frames share the link with those.
    ///
    /// With `per_session` off as well, a client started ahead is retired when
    /// it first leaves the front, like every other.
    pub lazy: bool,
    /// Once a switch of its own, whether the clients a dropped link took are
    /// started again as it comes back: `lazy` says now. Still read, so that a
    /// file that sets it loads, and never looked at.
    #[serde(skip_serializing)]
    pub revive: Retired,
    /// Once a switch of its own, whether a session nobody sees is drawn at
    /// most once a second: always so now (see
    /// [`crate::card::Command::Away`]). Still read, so that a file that sets
    /// it loads, and never looked at.
    #[serde(skip_serializing)]
    pub throttle: Retired,
    /// Scroll ahead of a slow link: a key that scrolls — `<C-e>`, `<C-d>`,
    /// `<C-f>`, `zz`, `zl` and their kind, in Normal mode or Insert — or a turn
    /// of the mouse wheel, either way, scrolls the window at once, before
    /// Neovim has heard of it. nvmux's own client only (see
    /// [`crate::client::predict`]).
    ///
    /// `"auto"` by default, as `predict_motions` and `predict_typing` have it
    /// too: on for a session on another machine, over ssh, and off for one on
    /// this machine. `"on"` is on for this machine's as well; `"off"`, every
    /// scroll waits for Neovim, whatever `predict_motions` and `predict_typing`
    /// say of the rest.
    ///
    /// On, a scroll is drawn ahead whatever the round trip: even one of 20 or
    /// 30 ms is felt, a key held down or typed fast. This machine's is a
    /// fraction of a millisecond, Neovim's own frame coming before a prediction
    /// could be seen, which is why `"auto"` leaves its sessions out: their
    /// clients leave the editor nothing to predict with, and time none of its
    /// answers.
    ///
    /// What it costs: the rows a scroll uncovers are drawn without their
    /// signs or virtual text until Neovim's own arrive, a round trip later,
    /// and in the colours tree-sitter and highlights on the buffer give them
    /// but not `:syntax`'s; and the agent the client leaves in the editor
    /// sends the text of the lines around each window's view over the link,
    /// and those colours, as the view moves — more of them the slower the
    /// link.
    pub predict_scrolls: Predict,
    /// Move the cursor ahead of a slow link, as `predict_scrolls` scrolls the
    /// window: the keys that move it in Normal mode are drawn as Neovim will
    /// move it. Each is drawn only where the client can work out exactly what
    /// Neovim will make of it, and judged against what Neovim says it did (see
    /// [`crate::client::predict::Sync`]): one Neovim made otherwise goes back
    /// to Neovim's at once.
    ///
    /// `"auto"` by default, as `predict_scrolls` has it: on for a session over
    /// ssh, and with `"on"` for one on this machine as well; `"off"`, each key
    /// waits for Neovim. The motions along a line — `h`, `l`, `0`, `^`, `$`,
    /// `g_`, `|` and their kind — and to another line — `j`, `k`, `+`, `-`,
    /// `_`, `<CR>` and theirs, and `j` and `k` mapped as LazyVim maps them —
    /// and by words, along a line or to another — `w`, `b`, `e` and `ge`, and
    /// `W`, `B`, `E` and `gE`, as `'iskeyword'` makes words — are drawn, with
    /// counts, and with them the numbers of `'relativenumber'` and the bar of
    /// `'cursorline'`: in view, or past `'scrolloff'`, the view scrolled as
    /// Neovim scrolls it, short of half the window, which Neovim centres the
    /// cursor for instead — but not to bring into view a line Neovim draws with
    /// virtual text or a sign, which the client would draw without them. With
    /// `predict_scrolls` as well, a key that scrolls, or a turn of the wheel,
    /// goes on from where the motions before it left the cursor, and they from
    /// it; and a scroll that takes the cursor to another line, the wheel's
    /// among them, has the numbers and the bar moved with it where the client
    /// can draw them as Neovim does, and leaves them as Neovim drew them till
    /// its next frame where not, as it always did. But not those to another
    /// line in a window whose gutter a `'statuscolumn'` draws, as LazyVim's is,
    /// which are left to Neovim. The client follows the rest by what Neovim
    /// says of each key rather than waiting for it to catch up with them all.
    ///
    /// What it costs, as `predict_typing` does too: the agent says where
    /// each key left the cursor, about 40 bytes a key back over the link,
    /// and as the view moves, which lines about it have virtual text or
    /// signs, a few bytes a run of them, having looked them over as Neovim
    /// went to wait for the key — a fraction of a millisecond where they are
    /// thick on every line; and each fence the client asks after its keys
    /// reads the agent's count, about 40 bytes more.
    pub predict_motions: Predict,
    /// Type ahead of a slow link: what is typed in Insert mode — a character
    /// put in, or taken out with `<BS>` — and `i`, `a`, `A`, `I` and the
    /// `<Esc>` after them are drawn as Neovim will draw them, the cursor taking
    /// the shape of the mode it goes into — judged, as `predict_motions`'s are,
    /// against what Neovim says it did.
    ///
    /// `"auto"` by default, as `predict_scrolls` has it: on for a session over
    /// ssh, and with `"on"` for one on this machine as well; `"off"`, each key
    /// waits for Neovim. Not a key a mapping or an abbreviation may make more
    /// of, one past `'textwidth'`, or one that indents the line again; nor, in
    /// a buffer whose plugins act on what is typed there, till enough of it has
    /// been found to go in as typed — or, whose plugins act as Insert mode is
    /// entered or left there, the keys that do that, till enough of them have
    /// been found to do no more than the key does: one that does more after
    /// that — a trimmer of the blanks lines end in finding some on another
    /// line — is drawn as the key alone does it, put right a round trip later,
    /// and has its kind prove itself again.
    ///
    /// What it costs: what `predict_motions` does, and the agent sends an
    /// edit with each key typed in Insert mode rather than at a pause, and
    /// the edit is the cursor's whole line: about a hundred bytes a key on a
    /// short line, and as much as four times the window's width on a long
    /// one — 800 bytes on a line of 800 characters, some 25 ms a key over a
    /// link of 256 kbit/s; and as a buffer is entered, or Insert mode in it,
    /// a few hundred bytes of what bears on typing there.
    pub predict_typing: Predict,
    /// Once which sessions' clients predicted, as each of `predict_scrolls`,
    /// `predict_motions` and `predict_typing` now says for itself: `"auto"`,
    /// those over ssh; `"on"`, this machine's as well; `"off"`, none. `"auto"`
    /// and `"on"` are still read, so that a file that sets them loads, and
    /// never looked at — `"on"` was no more than `"auto"` for any session whose
    /// round trip was long enough to predict over; `"off"` is refused in words
    /// that say which settings would do (see [`Superseded`]).
    #[serde(skip_serializing)]
    pub predict: Superseded,
    /// On a terminal wider than a window's text needs, draw the text in the
    /// middle of the window: a window wider than its text wants has it that
    /// wide, its lines wrapping there and its status line's words standing
    /// over it. nvmux's own client only (see `crate::client::wide`).
    ///
    /// `"off"`, the default, has every window's text as wide as the window,
    /// as Neovim's own client draws it. `"smart"` judges how wide each
    /// window's text is to be by what it shows: its buffer's `'textwidth'`
    /// where it has one, and room for its long lines where they are not
    /// wrapped — all of them with `'wrap'` off, and with it on those of a
    /// Markdown table or code block. `"fixed"` gives every window's text
    /// `window_width`.
    ///
    /// Either way, a window showing a terminal keeps its whole width: the
    /// program in it is told its window's width. So does a window whose
    /// text is in its middle already, as a start screen lays itself out.
    /// What Neovim draws after the end of a line — a language server's
    /// diagnostic, a blame — runs on into the window's margin, as far as
    /// the window goes, rather than stopping where the text does.
    pub centre: Centre,
    /// How many columns a window's text gets with `centre` on: every
    /// window's with `"fixed"`, and with `"smart"` one whose buffer has no
    /// `'textwidth'`, unless its long lines want more. 0 for no limit, which
    /// is `centre = "off"` whatever that says.
    pub window_width: u16,
    /// Once a switch of its own, whether each frame is written in as few
    /// bytes as it takes: always so now (see [`crate::client::screen`]).
    /// Still read, so that a file that sets it loads, and never looked at.
    #[serde(skip_serializing)]
    pub lean_output: Retired,
    /// Once a switch of its own, whether the terminal moves the rows a frame
    /// moved: always so now (see [`crate::client::region`]). Still read, so
    /// that a file that sets it loads, and never looked at.
    #[serde(skip_serializing)]
    pub scroll_regions: Retired,
    /// Once a switch of its own, whether a frame is put together from only
    /// the rows that changed, where it can be: always so now (see
    /// [`crate::client::App::frame`]). Still read, so that a file that sets
    /// it loads, and never looked at.
    #[serde(skip_serializing)]
    pub partial_frames: Retired,
}

/// A switch nvmux had once and has no more. A file that still sets it loads
/// as one that does not, rather than failing on a key nvmux does not know:
/// it is read as the switch it was, and never looked at again.
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
    /// What a client predicts, for a session on another machine if `remote`
    /// and on this one if not, as `predict_scrolls`, `predict_motions` and
    /// `predict_typing` say: each `"auto"` for the one and not the other.
    pub fn predictions(&self, remote: bool) -> Predictions {
        Predictions {
            scrolls: self.predict_scrolls.predicts(remote),
            motions: self.predict_motions.predicts(remote),
            typing: self.predict_typing.predicts(remote),
        }
    }

    /// Whether a client predicts anything at all, for a session on another
    /// machine if `remote`, and on this one if not.
    pub fn predicts(&self, remote: bool) -> bool {
        self.predictions(remote).any()
    }
}

/// What a client predicting draws ahead of Neovim, whatever the round trip:
/// as `[client]`'s `predict_scrolls`, `predict_motions` and `predict_typing`
/// say, each on its own, for the session it draws (see
/// [`ClientSettings::predictions`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Predictions {
    /// The keys that scroll, and the turns of the wheel.
    pub scrolls: bool,
    /// The keys that move the cursor in Normal mode.
    pub motions: bool,
    /// What is typed in Insert mode, and the keys that go in and out of it.
    pub typing: bool,
}

impl Default for Predictions {
    /// The scrolls alone: what a client predicts till it is told what its
    /// settings say (see `crate::client::App::with_predictions`), the agent
    /// left to say no more than the scrolls need.
    fn default() -> Self {
        Self {
            scrolls: true,
            motions: false,
            typing: false,
        }
    }
}

impl Predictions {
    /// Whether any is drawn ahead of Neovim.
    pub fn any(self) -> bool {
        self.scrolls || self.motions || self.typing
    }

    /// Whether the client goes by what the agent says Neovim made of each
    /// key (see [`crate::client::predict::Sync`]): for the motions and the
    /// typing, each of which goes on from where the one before it left the
    /// cursor. The scrolls alone wait for Neovim to catch up instead.
    pub fn acks(self) -> bool {
        self.motions || self.typing
    }
}

/// `[client] predict_scrolls`, `predict_motions` and `predict_typing`: for
/// which sessions a client draws its kind ahead of Neovim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Predict {
    /// For a session on another machine, over ssh, and not for one on this.
    #[default]
    Auto,
    /// For every session, this machine's too.
    On,
    /// For none: each key waits for Neovim.
    Off,
}

impl Predict {
    /// Every way, in the order the settings screen steps through them.
    pub const ALL: [Predict; 3] = [Predict::Auto, Predict::On, Predict::Off];

    /// The way as the file spells it.
    pub fn name(self) -> &'static str {
        match self {
            Predict::Auto => "auto",
            Predict::On => "on",
            Predict::Off => "off",
        }
    }

    /// Whether it is for a session on another machine if `remote`, and on
    /// this one if not.
    pub fn predicts(self, remote: bool) -> bool {
        match self {
            Predict::Auto => remote,
            Predict::On => true,
            Predict::Off => false,
        }
    }
}

// Read as the words the file spells them with (see `de_way`): `true` and
// `false`, as each was before it said which sessions, refused with the words
// that would do.

impl<'de> Deserialize<'de> for Predict {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        de_way(deserializer, &Predict::ALL, Predict::name)
    }
}

/// `[client] predict`, which said once which sessions' clients predicted, as
/// the file may still set it: see [`ClientSettings::predict`]. `"auto"` and
/// `"on"` are read as nothing, and `"off"` is refused with the settings that
/// would turn every prediction off — rather than read as nothing as well, which
/// would turn them all back on, or read as turning them all off, which a file
/// that sets one of them too could not say again: the settings screen, saving
/// one put back to its default, leaves it out.
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

/// `[client] centre`: each window's text in the middle of a window wider
/// than it needs, or not, and how wide it is there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Centre {
    /// As wide as the window, as Neovim's own client draws it.
    #[default]
    Off,
    /// In the middle, as wide as what the window shows wants.
    Smart,
    /// In the middle, `window_width` wide.
    Fixed,
}

impl Centre {
    /// Every way, in the order the settings screen steps through them.
    pub const ALL: [Centre; 3] = [Centre::Off, Centre::Smart, Centre::Fixed];

    /// The way as the file spells it.
    pub fn name(self) -> &'static str {
        match self {
            Centre::Off => "off",
            Centre::Smart => "smart",
            Centre::Fixed => "fixed",
        }
    }
}

// Read as the words the file spells them with (see `de_way`): anything else,
// the switch each of them once was among it, is refused with the words that
// would do.

impl<'de> Deserialize<'de> for Centre {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        de_way(deserializer, &Centre::ALL, Centre::name)
    }
}

/// One of `all`, read from the word `name` spells it with. Anything else is
/// refused in words that say which words would do — `expected "off",
/// "smart" or "fixed"` — where TOML's own for a setting that is no word at
/// all, `true` say, is "wanted string or table".
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

/// What nvmux's own client draws with: `[client]` and `[effects]`, of the
/// settings nvmux is running on, and whether the sessions are another
/// machine's, which every prediction goes by: a client predicts only for a
/// session over ssh (see [`ClientSettings::predicts`]).
///
/// Handed to a client as it starts, in its environment ([`Drawing::to_env`]),
/// and told again whenever they change (`card::Command::Settings`), rather
/// than read from the file by the client itself: the file does not have
/// what the settings screen has changed and not saved, nor where the
/// sessions are, which is the command line's to say.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Drawing {
    pub client: ClientSettings,
    pub effects: EffectsSettings,
    /// The sessions are on another machine, over ssh: see [`remote`].
    pub remote: bool,
}

impl Drawing {
    /// As a client's environment carries it: JSON, on one line.
    pub fn to_env(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// [`Drawing::to_env`] read back; `None` for anything else.
    pub fn from_env(text: &str) -> Option<Drawing> {
        serde_json::from_str(text).ok()
    }

    /// The settings a client runs on: these, and the defaults for what it
    /// has no use for.
    pub fn settings(&self) -> Settings {
        Settings {
            client: self.client,
            effects: self.effects,
            ..Settings::default()
        }
    }
}

impl Settings {
    /// What nvmux's own client draws with, of these settings, for the
    /// sessions this nvmux runs.
    pub fn drawing(&self) -> Drawing {
        Drawing {
            client: self.client,
            effects: self.effects,
            remote: remote(),
        }
    }
}

/// `[client] ui`: whose client draws a session.
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

/// How nvmux's own screens look: the colour they are drawn in (see
/// [`crate::theme`]), and whether the ones a key brings up over a session
/// float over it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StyleSettings {
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
    /// Whether the picker (`<prefix> Space`), the settings (`<prefix> ,`)
    /// and the help (`<prefix> ?`) float over the session they are brought
    /// up from, in a panel in the middle of it, the session going on behind
    /// them dimmed (see `crate::pty::panel`).
    ///
    /// On, the default. Off, each takes the whole terminal, the session
    /// dissolving out under it and back in after, as the picker always did.
    /// The picker's own `?` and `,` follow the picker: over the session in
    /// its place where it floats, and on the terminal it has where it does
    /// not. On a terminal too small for a panel and some of the session round
    /// it, one that would float takes the terminal too.
    ///
    /// Flipped in the settings, it moves them at once: floating, they come up
    /// full screen; full screen over a session, they come up floating over
    /// it.
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

/// `[style] color`: whose colour nvmux's own screens are drawn in.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SmearSettings {
    /// This effect's own switch, under `[effects] enabled`. Off unless turned
    /// on; off, the cursor jumps, as the terminal's does.
    pub enabled: bool,
}

/// nvmux's own client: what flies off the cursor as it moves — Neovide's
/// `cursor_vfx_mode` (see [`crate::client::anim::vfx`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ParticleSettings {
    /// This effect's own switch, under `[effects] enabled`. Off by default,
    /// as Neovide's is.
    pub enabled: bool,
    pub mode: ParticleMode,
}

/// `[effects.particles] mode`, spelt as Neovide spells `cursor_vfx_mode`.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WindowsSettings {
    /// This effect's own switch, under `[effects] enabled`.
    pub enabled: bool,
}

/// nvmux's own client: a blinking cursor fading out and back in rather than
/// flashing — Neovide's `cursor_smooth_blink` (see
/// [`crate::client::anim::blink`]). Only a block cursor fades; a bar or an
/// underline keeps the terminal's own blink.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BlinkSettings {
    /// This effect's own switch, under `[effects] enabled`. Off by default,
    /// as Neovide's is; and it does nothing for a cursor `'guicursor'` does
    /// not make blink, nor for a session out of sight, whose cursor holds
    /// still until it is back in front.
    pub enabled: bool,
}

/// The fade between screens (see [`crate::fade`]): each one dissolves into the
/// terminal's own background colour and the next dissolves up out of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
            quick_list: Retired,
            push_news: Retired,
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
    let t = StyleSettings::default();
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
         # float = {style_float:<10}   # the picker, settings and help over the session; false takes the screen\n\
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

/// How wide the template pads a command's name under `[keys.bind]`: as wide
/// as the longest, so their keys line up.
fn command_width() -> usize {
    crate::keys::BINDINGS
        .iter()
        .map(|b| b.hint.len())
        .max()
        .unwrap_or(0)
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

// --- saving ---------------------------------------------------------------

/// The file a save writes: the one [`load`] reads, or would read — so
/// `$NVMUX_CONFIG` when it is set. The first run never writes there, but a
/// save is something the user asked for.
fn config_path() -> Option<PathBuf> {
    let nvmux_config = std::env::var_os("NVMUX_CONFIG");
    let xdg = std::env::var_os("XDG_CONFIG_HOME");
    let home = std::env::var_os("HOME");
    match resolve_config_path(nvmux_config.as_deref(), xdg.as_deref(), home.as_deref()) {
        ConfigSource::Explicit(path) | ConfigSource::Default(path) => Some(path),
        ConfigSource::None => None,
    }
}

/// A path the way the user would type it: under their home, `~/` and the
/// rest.
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
    /// The config file.
    pub path: PathBuf,
    /// How many settings it changed in it: none when it already said what
    /// was being saved.
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

/// Write what `live` has changed from `saved` — what the file was last known
/// to hold — into the config file, and nothing else.
///
/// The file is read again first, since it may have been edited by hand while
/// nvmux ran: one that no longer reads is not written over, and a setting
/// being saved that the file has changed too is not overwritten unless
/// `overwrite` says so. Each setting is written where the file has it,
/// comments and all: in place if it is set, by uncommenting the template's
/// line for it if it is not, and otherwise at the end of its table. One set
/// back to its default is commented back out, so the file follows the default
/// again. With no file yet, the first-run template is written with the
/// changes in it.
///
/// Through a symlink to the file it names, keeping that file's mode (see
/// [`crate::paths::write_through`]).
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
    /// The text no longer reads as a config: where, and why, in a line.
    Unreadable(String),
    /// Settings being saved, by path, that the text has changed too.
    Changed(Vec<String>),
    /// No edit came out holding what was to be saved.
    Unedited,
}

/// The config file's `text` — `None` when there is none — with what `live`
/// has changed from `saved` written into it, and how many settings that
/// changed in it. See [`save`].
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
    // Changed in the file as well, to something other than what is being
    // saved: somebody's edit, which a save would quietly undo.
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

    // What the file is to hold: what it holds now, with each change made.
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

/// [`Edit`]s made to the text where they belong and nowhere else: every byte
/// not part of a change kept as it was.
///
/// A setting the file sets has its value replaced where it stands, its note
/// kept in its column — or, put back to its default, its line commented out.
/// One it does not set is uncommented where its table has it as a comment, as
/// the template does every setting, and otherwise added after its table's
/// last setting, or in a table of its own at the end. `None` where the file
/// is laid out in a way this does not edit — a table only spelt inline or in
/// dotted keys — which [`edit_structure`] is for.
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
            // A table the file spells some other way than a header of its
            // own: not one to add a header for.
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

/// Where the value of `key` in the table at `path` is in the original text,
/// and whether it is in an inline table — where a line cannot be commented
/// out — or `None` when the file does not set it.
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

/// Whether the table at `path` is in the file spelt some other way than a
/// header of its own: inline, or in dotted keys. A table that is only there
/// because a table under it is — `[effects]` with only `[effects.fade]` in
/// the file — is not: TOML lets its header be added after.
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

/// Every table header in the file — `[effects.fade]` — by its path, with
/// where it starts.
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

/// The table `table`'s header line, where it ends, and the lines under it
/// and over the next header — or `None` when the file has no header for it.
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
    // [`commented`] found the key with an `=` after it, so there is one.
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

/// What follows a value on its line, when it is a note — blanks then a `#`
/// comment — with the blanks made up for the new value's length, so the note
/// keeps its column. `None` when it is not a note.
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

/// Where a line added to a table goes: after its last setting, or straight
/// under its header when it has none — before any comments that lead into the
/// next table.
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

/// The edits made through `toml_edit`'s model of the file rather than its
/// text: for a table spelt inline or in dotted keys, which [`edit_in_place`]
/// leaves alone. Comments survive this too, though a value set back to its
/// default is removed rather than commented out.
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
        // A table only there for one under it gets a header now it has a
        // setting of its own.
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

/// Whether the sessions this nvmux runs are on another machine, over ssh.
static REMOTE: AtomicBool = AtomicBool::new(false);

/// Say whether the sessions are on another machine, over ssh, as `main`
/// does once, before it starts a client: what each client is told with its
/// settings, which `"auto"` predicts only for (see [`Drawing`]), and what
/// the settings screen checks a change of `[client] ui` against.
pub fn set_remote(remote: bool) {
    REMOTE.store(remote, Ordering::Relaxed);
}

/// Whether the sessions are on another machine, as [`set_remote`] said: not
/// until it has, which is a unit test's case, and a client's own.
pub fn remote() -> bool {
    REMOTE.load(Ordering::Relaxed)
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

    /// The scrolls, the motions and the typing are each `"auto"` unless told
    /// otherwise: predicted for a session over ssh, and not for one on this
    /// machine. `"on"` predicts a kind for both, and `"off"` for neither, each
    /// on its own or with any other. What a client is told comes back from its
    /// environment as it went; and a switch, as each was before it said which
    /// sessions, or any other word, is refused in words that say which would
    /// do.
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
        // What each says, over ssh and on this machine: the scrolls, the
        // motions and the typing.
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

    /// A file that still sets `predict`, which once said which sessions'
    /// clients predicted, loads as one that does not where it said those
    /// over ssh, or this machine's too — the round trip of neither ever
    /// long enough — and is refused where it said none, in words that name
    /// the settings that say so now: read as nothing, it would turn every
    /// prediction back on. Its switches of before are as wrong as any other
    /// word, and a client is not told it.
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

    /// A wide terminal is left alone unless told otherwise, and each window's
    /// text centred when it is, up to 120 columns or as wide as it wants:
    /// each way a word away, spelt as the template spells it. The switches
    /// they were, `centre` itself and `smart_width`, are errors.
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

    /// A file that still sets a switch nvmux has retired — what each said is
    /// what nvmux does now, or `lazy` says — loads as one that does not,
    /// whatever it says; one that sets it to anything but a switch is as
    /// wrong as ever. A client is not told them.
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

    /// The picker, the settings and the help float over the session unless
    /// told otherwise; full screen is a line away, and leaves the colour as it
    /// was.
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

    /// What a client draws with goes to it in its environment and comes back
    /// as it went, where its sessions are included, the rest of its settings
    /// the defaults.
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

    // --- saving (pure) ------------------------------------------------------

    /// The first-run template, as a first run with the default prefix wrote it.
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

    /// The lines of `after` that are not lines of `before`, and the other way
    /// round: what a save changed.
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

    /// A setting the template has as a comment is uncommented where it is,
    /// its note after it, and not a byte else changes.
    #[test]
    fn a_setting_the_template_comments_is_uncommented_in_place() {
        let before = template();
        let live = with(&Settings::default(), "keys.timeout_ms", Value::Number(1500));
        let (after, count) = save_over(Some(&before), &Settings::default(), &live);
        assert_eq!(count, 1);
        let (added, removed) = diff(&before, &after);
        // The note where it was, against the setting: the template pads the
        // number to six.
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

    /// Set back to its default, a setting is commented back out, so the file
    /// stops pinning it and follows the default again.
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

    /// A setting the file sets is changed where it is, its note kept in its
    /// column.
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

    /// With no file, the template is written with the change in it.
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

    /// A setting with no line anywhere goes after its table's last setting —
    /// and a table the file does not have goes at the end.
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

    /// A file written by hand, with comments, an inline table and dotted keys,
    /// keeps everything the save did not change.
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

    /// A table there only for one under it gets a header of its own.
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

    /// A setting being saved that the file changed too, since nvmux read it,
    /// is not overwritten unless asked again; one changed to the very value
    /// being saved is no clash at all.
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

    /// A file that no longer reads is not written over, and the refusal says
    /// where it stopped reading.
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

    /// Every setting, changed from the template and back, comes out of the
    /// file as nvmux would load it — and back at the template.
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
            // The prefix is the one line the template has set rather than
            // commented: back at its default, it is commented out as any is.
            let want = match setting.path().as_str() {
                "keys.prefix" => template().replace("\nprefix = ", "\n# prefix = "),
                _ => template(),
            };
            assert_eq!(back, want, "{}", setting.path());
        }
    }

    /// A command with quotes in it is written as TOML can spell it — a
    /// literal string, which needs no escapes — and reads back as typed.
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

    /// The edit keeps a file's line endings, notes and all.
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

    /// A save writes the file a load reads: `$NVMUX_CONFIG` when it is set,
    /// which the first run never writes but a save, being asked for, does.
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
