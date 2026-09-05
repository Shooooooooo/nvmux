//! Tracing setup.
//!
//! **Never to stdout.** From milestone 3 stdout belongs to the ratatui picker,
//! and from milestone 4 it belongs to the PTY relay, where a stray log line
//! would be written straight into the middle of the user's editor. So logs go
//! to a file from day one rather than being moved there later, once a log line
//! in the wrong place has already corrupted somebody's screen.
//!
//! Sessions have their own separate logs: `<runtime_dir>/<id>.log` holds the
//! headless server's own output. Headless Neovim writes `:echomsg` *and* errors
//! to stderr, which is why the spawn script redirects both.

use std::fs::OpenOptions;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

/// Start logging to `<dir>/nvmux.log`.
///
/// Verbosity comes from `$NVMUX_LOG` (same syntax as `RUST_LOG`), defaulting to
/// warnings only, so the file does not grow without anyone asking it to.
pub fn init(dir: &Path) -> Result<()> {
    let path = crate::config::client_log(dir);
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening log file {}", path.display()))?;

    let filter =
        EnvFilter::try_from_env("NVMUX_LOG").unwrap_or_else(|_| EnvFilter::new("nvmux=warn"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(Mutex::new(file))
        // No colour: this is a file, and escape codes in a log you `cat` while
        // debugging a terminal problem are their own small hell.
        .with_ansi(false)
        .with_target(true)
        .init();

    tracing::debug!(version = env!("CARGO_PKG_VERSION"), "nvmux started");
    Ok(())
}
