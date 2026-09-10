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
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

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
            timeout_ms: 500,
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
/// that was never really set.
fn nonempty(value: Option<&OsStr>) -> Option<&OsStr> {
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
         # command = {command:?}\n",
        prefix = crate::keys::prefix_label(prefix),
        timeout = k.timeout_ms,
        command = s.command,
    )
}

/// Write the first-run config to `path`, creating its parent directory.
///
/// Atomic (temp file plus `rename` within the directory), matching
/// [`crate::session::Session::write_atomic`]. The parent is created with a
/// gentle `DirBuilder`, not `paths::ensure_dir_secure`: that is `/tmp`
/// hardening that would reject a pre-existing `~/.config` at `0755` and does not
/// create missing parents.
pub fn write_default(path: &Path, prefix: u8) -> Result<(), ConfigError> {
    use std::io::Write;
    let err = |source| ConfigError::Write {
        path: path.to_path_buf(),
        source,
    };

    if let Some(parent) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(err)?;
    }

    let contents = render_default_config(prefix);
    let tmp = path.with_extension(format!("toml.tmp{}", std::process::id()));
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        err(e)
    })
}

static SETTINGS: OnceLock<Settings> = OnceLock::new();

/// Install the loaded settings, once, before anything reads them. `main` is the
/// only caller; a second call is a bug, so it warns and keeps the first.
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
            timeout_ms = 500\n\
            [session]\n\
            command = \"nvim --headless --listen {sock}\"\n";
        let s: Settings = toml::from_str(doc).expect("valid");
        assert_eq!(s, Settings::default());
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
    /// wrong response — an unknown table, an unknown key in the `[keys]` table,
    /// and an unparseable value all have to be refused rather than ignored.
    #[test]
    fn a_typo_is_never_silently_ignored() {
        for (what, doc) in [
            ("an unknown top-level table", "[colours]\nx = 1\n"),
            ("an unknown keys key", "[keys]\nprefx = \"C-a\"\n"),
            ("an unparseable prefix", "[keys]\nprefix = \"nope\"\n"),
            ("an unknown session key", "[session]\ncmd = \"nvim\"\n"),
        ] {
            assert!(
                toml::from_str::<Settings>(doc).is_err(),
                "{what} should be rejected: {doc:?}"
            );
        }
    }

    /// The timeout feeds a `poll` call, so it has a floor and a ceiling; the
    /// extreme values that pass are also pinned so the bounds stay generous.
    #[test]
    fn out_of_range_values_are_rejected_with_their_key_named() {
        let cases = [
            ("[keys]\ntimeout_ms = 0\n", "keys.timeout_ms"),
            ("[keys]\ntimeout_ms = 60001\n", "keys.timeout_ms"),
            // Past `c_int`: the value `poll` would have read as "block forever".
            ("[keys]\ntimeout_ms = 3000000000\n", "keys.timeout_ms"),
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
        for doc in ["[keys]\ntimeout_ms = 1\n", "[keys]\ntimeout_ms = 60000\n"] {
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
        // The timeout and the command are documentation, not active settings.
        assert!(rendered.contains("# timeout_ms = 500"));
        assert!(
            rendered.contains("# command = \"nvim --headless --listen {sock}\""),
            "the template must document the command: {rendered:?}"
        );
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
        let p = std::env::temp_dir().join(format!("nvmux-cfg-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("scratch dir");
        p
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
