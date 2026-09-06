//! Tracing setup.
//!
//! **Never to stdout**, which belongs to the picker and then to the PTY relay:
//! a stray log line would land in the middle of the user's editor.

use std::fs::OpenOptions;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

/// Start logging to `<dir>/nvmux.log`. Verbosity comes from `$NVMUX_LOG`, same
/// syntax as `RUST_LOG`, defaulting to warnings only.
pub fn init(dir: &Path) -> Result<()> {
    let path = crate::paths::client_log(dir);
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
        // No colour: escape codes in a log you `cat` while debugging a
        // terminal problem only make it harder to read.
        .with_ansi(false)
        .with_target(true)
        .init();

    tracing::debug!(version = env!("CARGO_PKG_VERSION"), "nvmux started");
    Ok(())
}
