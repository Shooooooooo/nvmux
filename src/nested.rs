//! The nested-launch guard, like tmux's `$TMUX` check.
//!
//! `scripts/spawn.sh` exports [`MARKER`] into each session's editor, so every
//! `:terminal` job inherits it. If set, nvmux is running inside a session:
//! the outer relay would swallow every `<prefix>`, and attaching to the current
//! session would stack a second UI on it. Unsetting the variable overrides.

use std::ffi::OsStr;

use anyhow::{bail, Result};

/// Holds the session's socket path (pids are reused, names are metadata).
/// Also spelled in `scripts/spawn.sh`; a test in [`crate::shell`] keeps the
/// two in sync.
pub const MARKER: &str = "NVMUX";

/// Refuse to run inside a session. Call before anything is spawned or drawn.
pub fn check() -> Result<()> {
    guard(std::env::var_os(MARKER).as_deref())
}

/// Takes `var_os` so a non-UTF-8 marker still refuses rather than failing
/// open. An empty value is allowed: `NVMUX= nvmux` is the documented way to
/// force a nested run, since a shell cannot unset a variable inline.
fn guard(marker: Option<&OsStr>) -> Result<()> {
    match marker {
        Some(sock) if !sock.is_empty() => bail!(
            "already inside an nvmux session ({})\n\
             hint: `<prefix> Space` opens the picker from inside a session; \
             unset ${MARKER} to run nvmux anyway",
            sock.to_string_lossy()
        ),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refusal(value: &OsStr) -> String {
        guard(Some(value))
            .expect_err("a marker is a session")
            .to_string()
    }

    #[test]
    fn no_marker_is_not_a_session() {
        assert!(guard(None).is_ok());
    }

    #[test]
    fn an_empty_marker_is_how_a_nested_run_is_forced() {
        assert!(guard(Some(OsStr::new(""))).is_ok());
    }

    /// The second line must match the crate's `hint:` error shape (see
    /// [`crate::error`]).
    #[test]
    fn a_marker_refuses_and_says_what_to_do_instead() {
        let msg = refusal(OsStr::new("/tmp/nvmux-1000/abcdefgh.sock"));
        let (said, hint) = msg.split_once('\n').expect("a hint on its own line");
        assert!(said.contains("/tmp/nvmux-1000/abcdefgh.sock"), "{msg}");
        assert!(hint.starts_with("hint: "), "{msg}");
        assert!(hint.contains("<prefix> Space"), "{msg}");
        assert!(hint.contains("$NVMUX"), "{msg}");
    }

    #[test]
    fn a_marker_that_is_not_utf8_still_refuses() {
        use std::os::unix::ffi::OsStrExt;
        let msg = refusal(OsStr::from_bytes(b"/tmp/nvmux-1000/\xff.sock"));
        assert!(msg.contains("already inside an nvmux session"), "{msg}");
    }
}
