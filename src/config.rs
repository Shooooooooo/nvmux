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
/// as `[keys]`-style sections.
///
/// Not `Copy`: `[session] command` owns a `String`. Nothing reads it by value —
/// [`get`] hands out a `&'static Settings` — so this costs nothing.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub keys: KeySettings,
    pub session: SessionSettings,
    pub client: ClientSettings,
    pub fade: FadeSettings,
    pub ssh: SshSettings,
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

/// The local `nvim --remote-ui` client that draws a session (see [`crate::pty`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClientSettings {
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
    /// Off by default, because a parked client is still a UI of its session's
    /// server: another UI on the same session — another nvmux, another
    /// machine — shares its grid with it, at the smaller of the two sizes, and
    /// every session visited keeps an idle client process until nvmux leaves.
    pub per_session: bool,
}

/// How `nvmux <host>` reaches the host (see [`crate::transport`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SshSettings {
    pub transport: SshTransport,
}

/// The two ways there are to reach a host over ssh. Both run the same scripts
/// in the same kind of shell over there; they differ in how the connection is
/// shared between the shell and every session's socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SshTransport {
    /// One `ControlMaster` per host, and a unix-socket forward added to it per
    /// session ([`crate::transport::remote`]). The default wherever ssh can
    /// multiplex.
    ControlMaster,
    /// One plain connection with nvmux's relay at the far end, multiplexing
    /// the shell and the sockets itself ([`crate::transport::relay`]). For an
    /// ssh that cannot multiplex — the only one on Windows — or a host that
    /// refuses unix-socket forwards.
    Relay,
}

impl Default for SshTransport {
    fn default() -> Self {
        // OpenSSH for Windows has no ControlMaster; see `crate::mux`.
        if cfg!(windows) {
            Self::Relay
        } else {
            Self::ControlMaster
        }
    }
}

impl SshTransport {
    /// As the file spells it.
    pub fn label(self) -> &'static str {
        match self {
            Self::ControlMaster => "control-master",
            Self::Relay => "relay",
        }
    }
}

/// The fade between screens (see [`crate::fade`]): each one dissolves into the
/// terminal's own background colour and the next dissolves up out of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FadeSettings {
    /// Master switch. Two things force the fade off whatever this says:
    /// `NO_COLOR`, because the effect paints explicit colours, and a terminal
    /// that does not answer the startup colour query, because there is then
    /// nothing to fade *to* — see [`crate::fade::enabled`].
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
    /// [`crate::shadow`]); off, a session hard-cuts both ways.
    ///
    /// That parse is also what the attach notice dissolves into, so off, the
    /// notice's box empties the cells it covers and waits for a repaint to
    /// fill them (see [`crate::announce`]).
    pub session: bool,
    /// Whether the quick `<prefix> ?` / `<prefix> c` excursions fade too. Off
    /// makes those snappier at the cost of consistency.
    pub excursions: bool,
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
            excursions: true,
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

/// Ceiling for `fade.duration_ms`. Two seconds is already a transition nobody
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
        if self.fade.duration_ms == 0 {
            // `enabled = false` is how the fade is turned off; a zero-length
            // fade would be the same thing spelled as a schedule with no frames.
            // One millisecond still halves to something rather than to nothing,
            // which is what [`FadeSettings::one_way`] is careful about.
            return Err("fade.duration_ms must be at least 1".into());
        }
        if self.fade.duration_ms > MAX_FADE_MS {
            // It feeds a `sleep`, on every screen a switch dissolves.
            return Err(format!("fade.duration_ms must be at most {MAX_FADE_MS}"));
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
        if cfg!(windows) && self.ssh.transport == SshTransport::ControlMaster {
            // Refused here rather than at the first `nvmux <host>`, where it
            // would surface as ssh's own "getsockname failed".
            return Err(
                "ssh.transport = \"control-master\" is not available on Windows, whose ssh \
                 cannot multiplex; \"relay\" is"
                    .into(),
            );
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

/// The home the default config and state paths are under: `$HOME`, and on
/// Windows — where a shell seldom sets it — `%USERPROFILE%` in its absence,
/// so the files are in the same place relative to it on every platform.
pub(crate) fn home() -> Option<std::ffi::OsString> {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty());
    #[cfg(windows)]
    let home = home.or_else(|| std::env::var_os("USERPROFILE"));
    home
}

/// Resolve the config path from the relevant environment, purely.
///
/// Precedence: `$NVMUX_CONFIG` (an exact path) > `$XDG_CONFIG_HOME/nvmux/` >
/// `$HOME/.config/nvmux/` (see [`home`]).
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
    let home = home();

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
    let home = home();
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
    let f = FadeSettings::default();
    let h = SshSettings::default();
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
         # One Neovim client per session, parked while another is in front, so a\n\
         # switch back puts the screen back at once. Each is one more UI on its\n\
         # session.\n\
         # per_session = {per_session}\n\
         \n\
         [fade]\n\
         # The dissolve between screens, and of the notice a switch puts up.\n\
         # NO_COLOR turns it off whatever this says.\n\
         # enabled     = {fade_enabled}\n\
         # duration_ms = {fade_duration}\n\
         # session     = {fade_session}\n\
         # excursions  = {fade_excursions}\n\
         \n\
         [ssh]\n\
         # How `nvmux <host>` reaches the host: \"control-master\" shares one ssh\n\
         # connection through a ControlMaster; \"relay\" makes one plain connection\n\
         # and multiplexes it itself, for an ssh that cannot (Windows) or a host\n\
         # that refuses unix-socket forwards.\n\
         # transport = {transport:?}\n",
        prefix = crate::keys::prefix_label(prefix),
        timeout = k.timeout_ms,
        command = s.command,
        per_session = c.per_session,
        fade_enabled = f.enabled,
        fade_duration = f.duration_ms,
        fade_session = f.session,
        fade_excursions = f.excursions,
        transport = h.transport.label(),
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

    #[test]
    fn a_full_document_at_the_defaults_round_trips() {
        let doc = "\
            [keys]\n\
            prefix = \"Ctrl-Space\"\n\
            timeout_ms = 1000\n\
            [session]\n\
            command = \"nvim --headless --listen {sock}\"\n\
            [client]\n\
            per_session = false\n\
            [fade]\n\
            enabled = true\n\
            duration_ms = 200\n\
            session = true\n\
            excursions = true\n";
        let s: Settings = toml::from_str(doc).expect("valid");
        assert_eq!(s, Settings::default());
        // The transport's default is the platform's, so it is spelled here as
        // the platform would spell it.
        let doc = format!(
            "{doc}[ssh]\ntransport = {:?}\n",
            SshSettings::default().transport.label()
        );
        let s: Settings = toml::from_str(&doc).expect("valid");
        assert_eq!(s, Settings::default());
    }

    /// Where ssh can multiplex, a ControlMaster; where it cannot, the relay —
    /// and the one Windows cannot use is refused by name, not left for ssh to
    /// refuse with words about sockets.
    #[test]
    fn the_transport_is_the_platforms_unless_asked_for() {
        let expected = if cfg!(windows) {
            SshTransport::Relay
        } else {
            SshTransport::ControlMaster
        };
        assert_eq!(Settings::default().ssh.transport, expected);
        let s: Settings = toml::from_str("[ssh]\ntransport = \"relay\"\n").expect("valid");
        assert_eq!(s.ssh.transport, SshTransport::Relay);
        assert!(toml::from_str::<Settings>("[ssh]\ntransport = \"mosh\"\n").is_err());
        let master = parse(
            Path::new("test.toml"),
            "[ssh]\ntransport = \"control-master\"\n",
        );
        if cfg!(windows) {
            match master.expect_err("refused on Windows") {
                ConfigError::Invalid { message, .. } => {
                    assert!(message.contains("ssh.transport"), "{message}")
                }
                other => panic!("expected Invalid, got {other:?}"),
            }
        } else {
            master.expect("fine where ssh multiplexes");
        }
    }

    #[test]
    fn a_partial_fade_table_keeps_the_other_fade_defaults() {
        let s: Settings = toml::from_str("[fade]\nduration_ms = 40\n").expect("valid");
        assert_eq!(s.fade.duration_ms, 40);
        assert_eq!(s.fade.enabled, FadeSettings::default().enabled);
        assert_eq!(s.fade.session, FadeSettings::default().session);
        assert_eq!(s.fade.excursions, FadeSettings::default().excursions);
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
        let s: Settings = toml::from_str("[fade]\nenabled = false\n").expect("valid");
        assert!(!s.fade.enabled);
    }

    /// Off unless asked for: a parked client is a UI its session's other users
    /// share a grid with, which nobody should get without having chosen it.
    #[test]
    fn one_client_per_session_is_off_unless_asked_for() {
        assert!(!Settings::default().client.per_session);
        let s: Settings = toml::from_str("[client]\nper_session = true\n").expect("valid");
        assert!(s.client.per_session);
        assert_eq!(s.fade, FadeSettings::default());
        assert_eq!(s.session, SessionSettings::default());
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
            ("an unknown fade key", "[fade]\nduration = 40\n"),
            ("an unknown client key", "[client]\nkeep = true\n"),
            ("an unparseable client value", "[client]\nper_session = 1\n"),
            ("an unparseable fade value", "[fade]\nenabled = \"yes\"\n"),
            ("an unknown ssh key", "[ssh]\nmultiplex = true\n"),
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
            ("[fade]\nduration_ms = 0\n", "fade.duration_ms"),
            ("[fade]\nduration_ms = 2001\n", "fade.duration_ms"),
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
            "[fade]\nduration_ms = 1\n",
            "[fade]\nduration_ms = 2000\n",
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
        assert!(rendered.contains("\n[fade]\n"), "{rendered:?}");
        assert!(rendered.contains("\n[client]\n"), "{rendered:?}");
        assert!(rendered.contains("\n[ssh]\n"), "{rendered:?}");
        assert!(
            rendered.contains(&format!(
                "# transport = {:?}",
                SshSettings::default().transport.label()
            )),
            "the template must document the transport: {rendered:?}"
        );
        for line in [
            "# per_session = false",
            "# enabled     = true",
            "# duration_ms = 200",
            "# session     = true",
            "# excursions  = true",
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
