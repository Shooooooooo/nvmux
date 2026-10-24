//! nvmux — a tmux-style session manager for Neovim. macOS and Linux only.
//!
//! The editor is drawn by a client on a PTY whose bytes pass through: nvmux's
//! own animating client ([`client`]) or `nvim --remote-ui`. Main modules:
//!
//! - [`ui`]: the session picker and [`ui::settings`].
//! - [`pty`]: the PTY proxy, watching stdin for the prefix ([`keys`],
//!   [`keyseq`]), with the [`hint`] bar.
//! - [`transport`]: local or SSH sessions; [`reconnect`] after a dropped link.
//! - [`notes`] and [`tell`]: per-session status for the picker and cards.
//! - [`card`], [`announce`], [`falloff`], [`theme`]: what nvmux draws itself.

// Docs link private items on purpose; they resolve under
// `cargo doc --document-private-items`.
#![allow(rustdoc::private_intra_doc_links)]

pub mod announce;
pub mod boundary;
pub mod card;
pub mod cli;
pub mod client;
pub mod config;
pub mod dirs;
pub mod error;
pub mod fade;
pub mod falloff;
pub mod handoff;
pub mod hint;
pub mod ids;
pub mod input;
pub mod keys;
pub mod keyseq;
pub mod launch;
pub mod ledger;
pub mod logging;
pub mod nested;
pub mod notes;
pub mod nvim;
pub mod palette;
pub mod panics;
pub mod paths;
pub mod pool;
pub mod proc;
pub mod pty;
pub mod reconnect;
pub mod rpc;
pub mod session;
pub mod settings;
pub mod shadow;
pub mod shell;
pub mod ssh;
pub mod state;
pub mod tell;
pub mod term;
pub mod theme;
pub mod transport;
pub mod ui;
pub mod winch;

#[cfg(test)]
pub(crate) mod test_support;

pub use error::{NvmuxError, Result};
