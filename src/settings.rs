//! The user's configuration file.
//!
//! nvmux runs with no configuration at all; this module is how it *optionally*
//! reads one. The governing idea is that a config file only ever *overrides* —
//! every default is sourced from the very [`const`](crate::fade::FRAMES) the
//! hard-wired code used, so an absent file, an empty file, and an omitted field
//! all reproduce the built-in behaviour byte for byte (see [`FadeSettings`] /
//! [`KeySettings`] `Default`).
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
//! [`load`] resolves the file (see [`resolve_config_path`]), reads and validates
//! it, and returns a [`Settings`]. `main` calls it once and hands the result to
//! [`init`]; everything else reads the process-global through [`get`]. `get`
//! falls back to [`Settings::default`] when nothing has been initialised, so a
//! unit test that never calls `init` reads the compiled defaults rather than a
//! developer's real `~/.config/nvmux/config.toml`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::Deserialize;

use crate::error::ConfigError;

/// The whole configuration. Every field has a table of its own so the file reads
/// as `[fade]` / `[keys]` sections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub fade: FadeSettings,
    pub keys: KeySettings,
}

/// The dip-to-black transition knobs (see [`crate::fade`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FadeSettings {
    /// Master switch. `NO_COLOR` still forces the effect off regardless — see
    /// [`crate::fade::enabled`].
    pub enabled: bool,
    /// Steps per direction; must be at least 1 (see [`Settings::validate`]).
    pub frames: usize,
    pub frame_delay_ms: u64,
    pub hold_ms: u64,
    pub excursions: bool,
    pub raw_dissolve: bool,
}

/// The prefix key and how long a half-typed sequence waits (see [`crate::keys`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KeySettings {
    /// The prefix, written as `"C-t"` / `"Ctrl-a"` in the file and parsed to its
    /// control byte by [`crate::keys::parse_prefix`].
    #[serde(deserialize_with = "de_prefix")]
    pub prefix: u8,
    pub timeout_ms: u64,
}

// The defaults are the current constants, so `Settings::default()` — which the
// container `#[serde(default)]` uses for every missing field — is exactly
// today's behaviour.

impl Default for FadeSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            frames: crate::fade::FRAMES,
            frame_delay_ms: crate::fade::FRAME_DELAY.as_millis() as u64,
            hold_ms: crate::fade::HOLD.as_millis() as u64,
            excursions: crate::fade::EXCURSIONS,
            raw_dissolve: crate::fade::RAW_FADE_DISSOLVE,
        }
    }
}

impl Default for KeySettings {
    fn default() -> Self {
        Self {
            prefix: crate::keys::PREFIX,
            timeout_ms: crate::keys::TIMEOUT.as_millis() as u64,
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

impl Settings {
    /// Rules that the deserializer cannot express. Kept minimal: only values that
    /// would misbehave at runtime, not taste.
    fn validate(&self) -> Result<(), String> {
        if self.fade.frames == 0 {
            // `step / frames` would be a division by zero -> NaN coverage.
            return Err("fade.frames must be at least 1".into());
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

    #[test]
    fn defaults_match_the_current_constants() {
        let f = FadeSettings::default();
        assert!(f.enabled);
        assert_eq!(f.frames, crate::fade::FRAMES);
        assert_eq!(
            f.frame_delay_ms,
            crate::fade::FRAME_DELAY.as_millis() as u64
        );
        assert_eq!(f.hold_ms, crate::fade::HOLD.as_millis() as u64);
        assert_eq!(f.excursions, crate::fade::EXCURSIONS);
        assert_eq!(f.raw_dissolve, crate::fade::RAW_FADE_DISSOLVE);

        let k = KeySettings::default();
        assert_eq!(k.prefix, crate::keys::PREFIX);
        assert_eq!(k.timeout_ms, crate::keys::TIMEOUT.as_millis() as u64);
    }

    #[test]
    fn an_empty_document_is_the_defaults() {
        let s: Settings = toml::from_str("").expect("empty is valid");
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn a_full_document_at_the_defaults_round_trips() {
        let doc = "\
            [fade]\n\
            enabled = true\n\
            frames = 8\n\
            frame_delay_ms = 12\n\
            hold_ms = 30\n\
            excursions = true\n\
            raw_dissolve = true\n\
            [keys]\n\
            prefix = \"Ctrl-t\"\n\
            timeout_ms = 500\n";
        let s: Settings = toml::from_str(doc).expect("valid");
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn a_partial_fade_table_keeps_the_other_fade_defaults() {
        let s: Settings = toml::from_str("[fade]\nframes = 4\n").expect("valid");
        assert_eq!(s.fade.frames, 4);
        assert_eq!(
            s.fade.frame_delay_ms,
            FadeSettings::default().frame_delay_ms
        );
        assert_eq!(s.fade.excursions, FadeSettings::default().excursions);
        assert_eq!(s.keys, KeySettings::default());
    }

    #[test]
    fn a_partial_keys_table_keeps_fade_defaults() {
        let s: Settings = toml::from_str("[keys]\ntimeout_ms = 250\n").expect("valid");
        assert_eq!(s.keys.timeout_ms, 250);
        assert_eq!(s.keys.prefix, KeySettings::default().prefix);
        assert_eq!(s.fade, FadeSettings::default());
    }

    #[test]
    fn fade_enabled_false_parses() {
        let s: Settings = toml::from_str("[fade]\nenabled = false\n").expect("valid");
        assert!(!s.fade.enabled);
    }

    #[test]
    fn a_remapped_prefix_parses_to_its_byte() {
        let s: Settings = toml::from_str("[keys]\nprefix = \"C-a\"\n").expect("valid");
        assert_eq!(s.keys.prefix, 0x01);
    }

    #[test]
    fn an_unknown_top_level_table_is_rejected() {
        assert!(toml::from_str::<Settings>("[colours]\nx = 1\n").is_err());
    }

    #[test]
    fn an_unknown_fade_key_is_rejected() {
        assert!(toml::from_str::<Settings>("[fade]\nframe = 4\n").is_err());
    }

    #[test]
    fn an_unknown_keys_key_is_rejected() {
        assert!(toml::from_str::<Settings>("[keys]\nprefx = \"C-a\"\n").is_err());
    }

    #[test]
    fn a_bad_prefix_string_is_a_parse_error() {
        assert!(toml::from_str::<Settings>("[keys]\nprefix = \"nope\"\n").is_err());
    }

    #[test]
    fn zero_frames_is_rejected() {
        let err = parse(Path::new("test.toml"), "[fade]\nframes = 0\n").expect_err("invalid");
        assert!(matches!(err, ConfigError::Invalid { .. }), "got {err:?}");
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
    /// only serialises these tests against each other. (Same shape as the
    /// `NO_COLOR` guard in `fade`.)
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
        std::fs::write(&file, "[fade]\nframes = 3\n").expect("write");
        env.set("NVMUX_CONFIG", &file);

        assert_eq!(load().expect("valid").fade.frames, 3);
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
        std::fs::write(cfg.join("config.toml"), "[fade]\nframe = 4\n").expect("write");
        env.set("HOME", &home);

        assert!(
            matches!(load(), Err(ConfigError::Parse { .. })),
            "a typo in the default file must not be silently ignored"
        );
        std::fs::remove_dir_all(&home).ok();
    }
}
