//! What differs by platform, under one set of names.
//!
//! The attach path — the relay in [`crate::pty`] — is a proxy between the
//! user's terminal and a `--remote-ui` client on a pty of its own, and almost
//! everything it does is the same wherever it runs: what it relays, what it
//! holds back, what it draws over a session. What differs is the machinery
//! underneath, and that is what lives here, behind the same names on both
//! sides:
//!
//! * [`Pty`] — the client and its pty: a Unix pty through `portable-pty`, or a
//!   Windows pseudoconsole (`windows::conpty`). Output to wait for and read,
//!   input to write, a size, and a process to hang up, reap and look at.
//! * [`Waiter`] — the relay's one wait: the terminal's input, the client's
//!   output and a resize, whichever comes first. A `poll` over three fds on
//!   Unix, one of them `SIGWINCH`'s self-pipe; a wait over the console's input
//!   and the pseudoconsole's output on Windows, where a resize is an input
//!   record.
//! * [`stop_pair`] and [`park_wait`] — how a parked client's thread is told to
//!   hand its client back, or to retire it (see [`crate::pty::Parked`]).
//! * [`TermInput`] — the terminal's replies to a query, read with a deadline
//!   (see [`crate::palette`]).

#[cfg(unix)]
pub(crate) mod unix;
#[cfg(windows)]
pub mod windows;

#[cfg(unix)]
pub use unix::PtySize;
#[cfg(unix)]
pub(crate) use unix::{park_wait, stop_pair, Command, Pty, StopRx, StopTx, TermInput, Waiter};
#[cfg(windows)]
pub use windows::conpty::PtySize;
#[cfg(windows)]
pub(crate) use windows::conpty::{Command, Pty};
#[cfg(windows)]
pub(crate) use windows::relay::{park_wait, stop_pair, StopRx, StopTx, TermInput, Waiter};

/// What a look at the client found, without changing anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Peek {
    Running,
    /// Stopped by a signal — a Unix state only.
    #[cfg_attr(windows, allow(dead_code))]
    Stopped,
    Gone,
}

/// What the relay's idle look at its client found, once it has revived a
/// client that had stopped itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChildState {
    Running,
    Gone,
}

/// What one of the relay's waits found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Wake {
    /// The terminal has typed something.
    pub(crate) stdin: bool,
    /// The client has written something, or gone.
    pub(crate) child: bool,
    /// The terminal changed size.
    pub(crate) resized: bool,
    /// Nothing at all happened before the timeout.
    pub(crate) idle: bool,
}

/// What a parked client's wait found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ParkWake {
    /// The owner has said something: hand the client back, or retire it.
    pub(crate) stop: bool,
    /// The client has written something, or gone.
    pub(crate) output: bool,
    /// Nothing happened before the timeout.
    pub(crate) idle: bool,
}

/// What a parked client's owner asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Order {
    /// Hang the client up and reap it, on the parked thread.
    Retire,
    /// Hand the client back, for the relay.
    Back,
}
