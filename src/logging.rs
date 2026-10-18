//! Tracing setup.
//!
//! **Never to stdout**, which belongs to the picker and then to the PTY relay:
//! a stray log line would land in the middle of the user's editor.

use std::fs::OpenOptions;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use tracing_subscriber::filter::Targets;
use tracing_subscriber::prelude::*;

/// Start logging to `<dir>/nvmux.log`, at the verbosity `$NVMUX_LOG` asks
/// for (see [`filter`]).
pub fn init(dir: &Path) -> Result<()> {
    let path = crate::paths::client_log(dir);
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening log file {}", path.display()))?;

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(Mutex::new(file))
                // No colour: escape codes in a log you `cat` while debugging a
                // terminal problem only make it harder to read.
                .with_ansi(false)
                .with_target(true),
        )
        .with(filter(std::env::var("NVMUX_LOG").ok().as_deref()))
        .init();

    tracing::debug!(version = env!("CARGO_PKG_VERSION"), "nvmux started");
    Ok(())
}

/// What gets logged, from `$NVMUX_LOG`: a comma-separated list of
/// `target=level`, or a bare level for every target, as `RUST_LOG` spells
/// them — `nvmux=debug`, `nvmux::pty=trace,warn`. Warnings from nvmux only
/// when it is not set or does not parse, and nothing at all when it is set
/// but empty.
///
/// `RUST_LOG`'s span and field filters (`target[span{field=value}]=level`)
/// are not taken: nvmux makes no spans, and parsing them is what would bring
/// in a regex engine.
fn filter(spec: Option<&str>) -> Targets {
    let warnings = || Targets::new().with_target("nvmux", tracing::Level::WARN);
    let Some(spec) = spec else {
        return warnings();
    };
    // Empty directives are skipped: `Targets` would read one as a target
    // that every event's begins with, and log everything.
    let directives: Vec<&str> = spec.split(',').filter(|d| !d.is_empty()).collect();
    if directives.is_empty() {
        return Targets::new();
    }
    directives.join(",").parse().unwrap_or_else(|_| warnings())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::Level;

    /// Which of these targets `spec` logs at `level`.
    fn logs(spec: Option<&str>, level: Level, targets: &[&str]) -> Vec<bool> {
        let filter = filter(spec);
        targets
            .iter()
            .map(|t| filter.would_enable(t, &level))
            .collect()
    }

    /// Unset, nvmux warns and nothing else says a word.
    #[test]
    fn unset_is_nvmux_warnings_only() {
        assert_eq!(
            logs(None, Level::WARN, &["nvmux", "nvmux::pty", "vte"]),
            [true, true, false]
        );
        assert_eq!(logs(None, Level::INFO, &["nvmux"]), [false]);
    }

    /// A target's level holds for the modules under it, the most specific
    /// target wins, and a bare level is for every target else.
    #[test]
    fn targets_and_levels_read_as_rust_log_spells_them() {
        let targets = ["nvmux", "nvmux::pty", "nvmux::shadow", "vte"];
        assert_eq!(
            logs(Some("nvmux=debug"), Level::DEBUG, &targets),
            [true, true, true, false]
        );
        let spec = Some("nvmux::pty=trace,warn");
        assert_eq!(
            logs(spec, Level::TRACE, &targets),
            [false, true, false, false]
        );
        assert_eq!(logs(spec, Level::WARN, &targets), [true, true, true, true]);
        assert_eq!(logs(Some("DEBUG"), Level::DEBUG, &targets), [true; 4]);
    }

    /// An empty directive — a trailing comma, or nothing at all — is no
    /// directive, as `RUST_LOG` has it: never a target that matches every
    /// event and logs it.
    #[test]
    fn an_empty_directive_logs_nothing_of_its_own() {
        assert_eq!(
            logs(Some("nvmux=debug,"), Level::TRACE, &["nvmux", "vte"]),
            [false, false]
        );
        for spec in ["", ","] {
            assert_eq!(
                logs(Some(spec), Level::ERROR, &["nvmux", "vte"]),
                [false, false],
                "{spec:?}"
            );
        }
    }

    /// What does not parse is as good as not set.
    #[test]
    fn what_does_not_parse_is_nvmux_warnings_only() {
        for spec in ["nvmux=verbose", "nvmux=debug=trace"] {
            assert_eq!(
                logs(Some(spec), Level::WARN, &["nvmux", "vte"]),
                [true, false],
                "{spec:?}"
            );
            assert_eq!(
                logs(Some(spec), Level::INFO, &["nvmux"]),
                [false],
                "{spec:?}"
            );
        }
    }
}
