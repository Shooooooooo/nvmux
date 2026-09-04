//! Terminal mode handling for the attach path. Milestone 4.
//!
//! # The contract
//!
//! On attach nvmux leaves the alternate screen and clears it, but **stays in raw
//! mode with `ISIG` disabled**. That is what makes `Ctrl-c`, `Ctrl-z` and
//! `Ctrl-s` arrive at Neovim as ordinary bytes instead of being turned into
//! signals for nvmux.
//!
//! Note that `cfmakeraw` already clears `ISIG` and `IXON`, sets `VMIN = 1` and
//! `VTIME = 0`, and disables `ICANON`, `ECHO` and `OPOST` — measured, not
//! assumed. The explicit `c_cc[VSUSP] = _POSIX_VDISABLE` is the part that is not
//! redundant, and `_POSIX_VDISABLE` is available on macOS as well as Linux.
//!
//! # The failure that actually matters
//!
//! Restoring the terminal on a *signal-initiated* exit is not covered by a
//! `Drop` guard or a panic hook. Because `cfmakeraw` clears `ISIG`, `^C` cannot
//! even reach nvmux; but a plain `kill` from another window then leaves the
//! user's shell in raw mode with no echo, which is the worst thing this tool
//! could do to someone. Milestone 4 must install SIGTERM/SIGHUP/SIGINT handling
//! that `tcsetattr`s the saved termios back before exiting.
//!
//! The type names are easy to get wrong: it is
//! `nix::sys::termios::SpecialCharacterIndices` (plural), and
//! `tcgetattr`/`tcsetattr` take an `impl AsFd`, not a `RawFd`.
