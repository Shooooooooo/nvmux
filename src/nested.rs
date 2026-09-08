//! The nested-launch guard.
//!
//! A session's editor is what starts a `:terminal` shell, so `scripts/spawn.sh`
//! exports [`MARKER`] into the `nvim --headless --listen` it spawns, and every
//! job that editor starts inherits it. Finding it set means this nvmux was
//! launched from *inside* a session — where the outer relay swallows every
//! `<prefix>` before an inner one could ever see it, so an inner nvmux could be
//! neither detached from nor left, its picker would paint over a terminal
//! buffer the outer client also owns, and attaching to the session you are
//! already in would stack a second UI on the server rendering the buffer you
//! are typing in.
//!
//! tmux's `$TMUX` rule, including the way out of it: unset the variable.

use std::ffi::OsStr;

use anyhow::{bail, Result};

/// Set in the environment of every session's editor, and so of everything that
/// editor starts. The value is the session's socket path: pids are reused and
/// names are only metadata, so the socket is what identifies a session.
///
/// `scripts/spawn.sh` writes it and this module reads it; a test in
/// [`crate::shell`] keeps the two spellings from drifting apart.
pub const MARKER: &str = "NVMUX";

/// Refuse to run inside a session. Called before anything is read, spawned or
/// drawn.
pub fn check() -> Result<()> {
    guard(std::env::var_os(MARKER).as_deref())
}

/// The rule itself, separated from the environment so it can be tested.
///
/// `var_os` rather than `var`: a marker that is not UTF-8 is still a marker,
/// and treating it as absent would fail open. An *empty* value is deliberately
/// not a session — `NVMUX= nvmux` is the documented way to force a nested run,
/// and a shell has no way to spell "unset" inline.
fn guard(marker: Option<&OsStr>) -> Result<()> {
    match marker {
        Some(sock) if !sock.is_empty() => bail!(
            "already inside an nvmux session ({})\n\
             hint: `<prefix> t` opens the picker from inside a session; \
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

    /// The documented override. `unset NVMUX` is a statement, not something a
    /// shell can put in front of one command, so `NVMUX= nvmux` has to work.
    #[test]
    fn an_empty_marker_is_how_a_nested_run_is_forced() {
        assert!(guard(Some(OsStr::new(""))).is_ok());
    }

    /// The message has one job beyond refusing: saying what to do instead. The
    /// second line is pinned to the `hint:` shape the rest of the crate's
    /// errors use — see [`crate::error`].
    #[test]
    fn a_marker_refuses_and_says_what_to_do_instead() {
        let msg = refusal(OsStr::new("/tmp/nvmux-1000/abcdefgh.sock"));
        let (said, hint) = msg.split_once('\n').expect("a hint on its own line");
        assert!(said.contains("/tmp/nvmux-1000/abcdefgh.sock"), "{msg}");
        assert!(hint.starts_with("hint: "), "{msg}");
        // Both ways on: the picker from inside, or unsetting the marker.
        assert!(hint.contains("<prefix> t"), "{msg}");
        assert!(hint.contains("$NVMUX"), "{msg}");
    }

    /// A marker nvmux cannot decode is still a marker. `std::env::var` would
    /// hand back `Err` here and the guard would fail open — which is why this
    /// reads `var_os`.
    #[test]
    fn a_marker_that_is_not_utf8_still_refuses() {
        use std::os::unix::ffi::OsStrExt;
        let msg = refusal(OsStr::from_bytes(b"/tmp/nvmux-1000/\xff.sock"));
        assert!(msg.contains("already inside an nvmux session"), "{msg}");
    }
}
