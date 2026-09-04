//! Driving the `ssh` client. Milestone 5.
//!
//! # Why a subprocess and not a Rust SSH library
//!
//! nvmux shells out to the system `ssh` so that `~/.ssh/config`, `ProxyJump`,
//! agent forwarding and hardware keys all work unmodified. Reimplementing that
//! surface is a project in itself. The `openssh` crate is also unsuitable: it
//! does not cleanly expose `ssh -O forward`, which is the whole mechanism here.
//!
//! # Shape
//!
//! One `ControlMaster` per host, reused for everything:
//!
//! ```text
//! ssh -M -N -f -o ControlMaster=yes -o ControlPath=<short> \
//!     -o ControlPersist=60 -o ExitOnForwardFailure=yes \
//!     -o ServerAliveInterval=15 -o ServerAliveCountMax=3 \
//!     -o StreamLocalBindUnlink=yes <host>
//! ```
//!
//! then per session, onto the *existing* master with no reconnection:
//!
//! ```text
//! ssh -o ControlPath=<ctl> -O forward -L <local_sock>:<remote_sock> <host>
//! ```
//!
//! with `-O cancel` to remove one and `-O check` to test the master.
//!
//! # Two things verified the hard way
//!
//! * `StreamLocalBindUnlink=yes` must be on the **master** invocation. Setting
//!   it on the `-O forward` client does nothing, because the master performs the
//!   bind. Without it: `-O cancel` exits 0 but leaves the local socket file on
//!   disk, and the next `-O forward` onto that path fails with rc 255 and
//!   `mux_client_forward: forwarding request failed`. That breaks
//!   detach-then-reattach, which is the flow this tool exists for. Measured both
//!   ways. nvmux also unlinks the local path itself before every forward, since
//!   belt and braces costs one syscall.
//! * `ssh -O forward` to a *nonexistent* remote socket still exits 0 and creates
//!   a working local socket. A successful forward is therefore no evidence that
//!   the remote Neovim is alive; only an RPC round trip through it is.
//!
//! `ControlPath` is computed by nvmux rather than left to ssh's `%C`/`%h%p%r`
//! tokens, which expand to unpredictable lengths — see [`crate::config`].

/// The minimum ssh nvmux supports: 6.7 added unix-socket forwarding.
pub use crate::nvim::MIN_SSH_VERSION;
