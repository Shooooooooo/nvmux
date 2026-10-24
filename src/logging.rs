//! Tracing setup.
//!
//! **Never to stdout**: it belongs to the picker and the PTY relay, so a stray
//! log line would land in the user's editor.

use std::fs::OpenOptions;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use tracing_subscriber::filter::Targets;
use tracing_subscriber::prelude::*;

/// Log to `<dir>/nvmux.log` at the verbosity `$NVMUX_LOG` sets (see
/// [`filter`]).
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
                // No colour: escape codes clutter a log read while debugging
                // terminal problems.
                .with_ansi(false)
                .with_target(true),
        )
        .with(filter(std::env::var("NVMUX_LOG").ok().as_deref()))
        .init();

    tracing::debug!(version = env!("CARGO_PKG_VERSION"), "nvmux started");
    Ok(())
}

/// Parse `$NVMUX_LOG`: comma-separated `target=level` or bare levels, as in
/// `RUST_LOG` (`nvmux::pty=trace,warn`). Unset or unparsable: nvmux warnings
/// only. Set but empty: nothing.
///
/// Span and field filters are not supported: nvmux makes no spans, and
/// parsing them would need a regex engine.
fn filter(spec: Option<&str>) -> Targets {
    let warnings = || Targets::new().with_target("nvmux", tracing::Level::WARN);
    let Some(spec) = spec else {
        return warnings();
    };
    // Skip empty directives: `Targets` treats one as a prefix of every
    // target and logs everything.
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

    fn logs(spec: Option<&str>, level: Level, targets: &[&str]) -> Vec<bool> {
        let filter = filter(spec);
        targets
            .iter()
            .map(|t| filter.would_enable(t, &level))
            .collect()
    }

    #[test]
    fn unset_is_nvmux_warnings_only() {
        assert_eq!(
            logs(None, Level::WARN, &["nvmux", "nvmux::pty", "vte"]),
            [true, true, false]
        );
        assert_eq!(logs(None, Level::INFO, &["nvmux"]), [false]);
    }

    /// A target covers its submodules, the most specific target wins, and a
    /// bare level covers all other targets.
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

    /// A trailing comma or empty spec must not match every target.
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
